// Copyright (c) Kyutai, all rights reserved.
// This source code is licensed under the license found in the
// LICENSE file in the root directory of this source tree.

use crate::streaming::{StreamTensor, StreamingModule};
use candle::{DType, Device, IndexOp, Module, Result, Tensor, D};
use candle_nn::{linear_no_bias, Linear, VarBuilder};
use std::sync::Arc;

fn linear(in_d: usize, out_d: usize, bias: bool, vb: VarBuilder) -> Result<Linear> {
    if bias {
        candle_nn::linear(in_d, out_d, vb)
    } else {
        linear_no_bias(in_d, out_d, vb)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PositionalEmbedding {
    Rope,
    Sin,
    None,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CrossAttention {
    Normal,
    Gated,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub d_model: usize,
    pub num_heads: usize,
    pub num_layers: usize,
    pub causal: bool,
    pub norm_first: bool,
    pub bias_ff: bool,
    pub bias_attn: bool,
    pub layer_scale: Option<f64>,
    pub positional_embedding: PositionalEmbedding,
    pub use_conv_block: bool,
    pub cross_attention: Option<CrossAttention>,
    pub conv_kernel_size: usize,
    pub use_conv_bias: bool,
    pub gating: Option<candle_nn::Activation>,
    pub norm: crate::NormType,
    pub context: usize,
    pub max_period: usize,
    pub max_seq_len: usize,

    pub kv_repeat: usize,
    pub dim_feedforward: usize,
    pub conv_layout: bool,
}

#[derive(Debug, Clone)]
pub struct RotaryEmbedding {
    sin: Tensor,
    cos: Tensor,
    span: tracing::Span,
}

impl RotaryEmbedding {
    pub fn new(dim: usize, max_seq_len: usize, theta: f32, dev: &Device) -> Result<Self> {
        let inv_freq: Vec<_> =
            (0..dim).step_by(2).map(|i| 1f32 / theta.powf(i as f32 / dim as f32)).collect();
        let inv_freq_len = inv_freq.len();
        let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?;
        let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
            .to_dtype(DType::F32)?
            .reshape((max_seq_len, 1))?;
        let freqs = t.matmul(&inv_freq)?;
        Ok(Self {
            sin: freqs.sin()?,
            cos: freqs.cos()?,
            span: tracing::span!(tracing::Level::TRACE, "rot"),
        })
    }

    pub fn apply_rotary_emb(&self, qk: &Tensor, seqlen_offset: usize) -> Result<Tensor> {
        let _enter = self.span.enter();
        let (_b_size, _nheads, seqlen, _headdim) = qk.dims4()?;
        let qk_dtype = qk.dtype();
        let c = self.cos.narrow(0, seqlen_offset, seqlen)?;
        let s = self.sin.narrow(0, seqlen_offset, seqlen)?;
        candle_nn::rotary_emb::rope_i(&qk.to_dtype(DType::F32)?, &c, &s)?.to_dtype(qk_dtype)
    }
}

#[derive(Debug, Clone)]
pub struct LayerScale {
    scale: Tensor,
}

impl LayerScale {
    pub fn new(d_model: usize, _init: f64, vb: VarBuilder) -> Result<Self> {
        let scale = vb.get(d_model, "scale")?;
        Ok(Self { scale })
    }
}

impl Module for LayerScale {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        xs.broadcast_mul(&self.scale)
    }
}

pub(crate) fn get_mask(
    size1: usize,
    size2: usize,
    context: usize,
    device: &Device,
) -> Result<Tensor> {
    let mask: Vec<_> = (0..size1)
        .flat_map(|i| {
            (0..size2)
                .map(move |j| u8::from(size1 + j > size2 + i || size1 + j + context < size2 + i))
        })
        .collect();
    Tensor::from_slice(&mask, (size1, size2), device)
}

#[derive(Debug, Clone)]
pub struct StreamingMultiheadAttention {
    in_proj: Linear,
    out_proj: Linear,
    kv_repeat: usize,
    num_heads: usize,
    context: usize,
    neg_inf: Tensor,
    rope: Option<Arc<RotaryEmbedding>>,
    kv_cache: candle_nn::kv_cache::KvCache,
    pos: usize,
    use_flash_attn: bool,
    span: tracing::Span,
}

impl StreamingMultiheadAttention {
    pub fn new(rope: &Option<Arc<RotaryEmbedding>>, cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let embed_dim = cfg.d_model;
        let num_kv = cfg.num_heads / cfg.kv_repeat;
        let out_dim = embed_dim + 2 * num_kv * (embed_dim / cfg.num_heads);
        let in_proj_weight = vb.get((out_dim, embed_dim), "in_proj_weight")?;
        let in_proj_bias =
            if cfg.bias_attn { Some(vb.get(out_dim, "in_proj_bias")?) } else { None };
        let in_proj = Linear::new(in_proj_weight, in_proj_bias);
        let out_proj = linear(embed_dim, embed_dim, cfg.bias_attn, vb.pp("out_proj"))?;
        let neg_inf = Tensor::new(f32::NEG_INFINITY, vb.device())?.to_dtype(vb.dtype())?;
        Ok(Self {
            in_proj,
            out_proj,
            rope: rope.clone(),
            kv_repeat: cfg.kv_repeat,
            num_heads: cfg.num_heads,
            context: cfg.context,
            neg_inf,
            kv_cache: candle_nn::kv_cache::KvCache::new(2, cfg.max_seq_len),
            pos: 0,
            use_flash_attn: false,
            span: tracing::span!(tracing::Level::TRACE, "mha"),
        })
    }

    pub fn forward(&mut self, xs: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let _enter = self.span.enter();
        if self.kv_repeat != 1 {
            candle::bail!("only kv-repeat = 1 is supported")
        }
        let (b, t, hd) = xs.dims3()?;
        let head_dim = hd / self.num_heads;
        // time_dim = 1, layout: b,t,h,d
        let qkv = xs.apply(&self.in_proj)?.reshape((b, t, 3, self.num_heads, head_dim))?;
        let q = qkv.i((.., .., 0))?;
        let k = qkv.i((.., .., 1))?;
        let v = qkv.i((.., .., 2))?;
        // qk_layer_norm = None
        // kv_repeat = 1, otherwise we would need repeat_kv
        let mut q = q.transpose(1, 2)?.contiguous()?; // b,h,t,d
        let mut k = k.transpose(1, 2)?.contiguous()?; // b,h,k,d
        let v = v.transpose(1, 2)?.contiguous()?; // b,h,k,d
        if let Some(rope) = &self.rope {
            q = rope.apply_rotary_emb(&q, self.pos)?;
            k = rope.apply_rotary_emb(&k, self.pos)?;
        }

        let (k, v) = {
            self.pos += k.dim(2)?;
            self.kv_cache.append(&k.contiguous()?, &v.contiguous()?)?
        };
        // The KV cache keeps all the data at the moment, we want to trim
        // down the part that comes from the cache to at most context to
        // be coherent with the mask shape we provide.
        let k_len = k.dim(2)?;
        let k_target_len = t + usize::min(self.context, k_len - t);
        let (k, v) = if k_target_len < k_len {
            let k = k.narrow(2, k_len - k_target_len, k_target_len)?;
            let v = v.narrow(2, k_len - k_target_len, k_target_len)?;
            (k, v)
        } else {
            (k.clone(), v.clone())
        };

        let xs = if q.dtype() == DType::BF16 && self.use_flash_attn {
            let q = q.transpose(1, 2)?;
            let k = k.transpose(1, 2)?;
            let v = v.transpose(1, 2)?;
            let softmax_scale = 1f32 / (head_dim as f32).sqrt();
            flash_attn(&q, &k, &v, softmax_scale, t > 1)?.transpose(1, 2)?
        } else {
            let pre_ws = q.matmul(&k.t()?)?; // b,h,t,k
            let pre_ws = (pre_ws * (head_dim as f64).powf(-0.5))?;

            let pre_ws = match mask {
                None => pre_ws,
                Some(mask) => {
                    let mask = mask.broadcast_left((b, self.num_heads))?;
                    let neg_inf = self.neg_inf.broadcast_as(pre_ws.shape())?;
                    mask.where_cond(&neg_inf, &pre_ws)?
                }
            };

            let ws = candle_nn::ops::softmax_last_dim(&pre_ws)?; // b,h,t,k
            ws.matmul(&v)? // b,h,t,d
        };
        let xs = xs
            .transpose(1, 2)? // b,t,h,d
            .reshape((b, t, hd))?
            .apply(&self.out_proj)?;
        Ok(xs)
    }

    pub fn reset_kv_cache(&mut self) {
        self.kv_cache.reset()
    }

    pub fn set_kv_cache(&mut self, kv_cache: candle_nn::kv_cache::KvCache) {
        self.kv_cache = kv_cache
    }
}

#[derive(Debug, Clone)]
pub struct StreamingMultiheadCrossAttention {
    in_proj_q: Linear,
    in_proj_k: Linear,
    in_proj_v: Linear,
    out_proj: Linear,
    kv_repeat: usize,
    num_heads: usize,
    neg_inf: Tensor,
    tanh_gate_alpha: Option<Tensor>,
    span: tracing::Span,
}

impl StreamingMultiheadCrossAttention {
    pub fn new(ca: CrossAttention, cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let embed_dim = cfg.d_model;
        let num_kv = cfg.num_heads / cfg.kv_repeat;
        let kv_dim = num_kv * (embed_dim / cfg.num_heads);
        let out_dim = embed_dim + 2 * kv_dim;
        let in_proj_weight = vb.get((out_dim, embed_dim), "in_proj_weight")?;
        let in_proj_weight_q = in_proj_weight.narrow(0, 0, embed_dim)?;
        let in_proj_weight_k = in_proj_weight.narrow(0, embed_dim, kv_dim)?;
        let in_proj_weight_v = in_proj_weight.narrow(0, embed_dim + kv_dim, kv_dim)?;
        let (in_proj_bias_q, in_proj_bias_k, in_proj_bias_v) = if cfg.bias_attn {
            let b = vb.get(out_dim, "in_proj_bias")?;
            let q = b.narrow(0, 0, embed_dim)?;
            let k = b.narrow(0, embed_dim, kv_dim)?;
            let v = b.narrow(0, embed_dim + kv_dim, kv_dim)?;
            (Some(q), Some(k), Some(v))
        } else {
            (None, None, None)
        };
        let in_proj_q = Linear::new(in_proj_weight_q, in_proj_bias_q);
        let in_proj_k = Linear::new(in_proj_weight_k, in_proj_bias_k);
        let in_proj_v = Linear::new(in_proj_weight_v, in_proj_bias_v);
        let out_proj = linear(embed_dim, embed_dim, cfg.bias_attn, vb.pp("out_proj"))?;
        let neg_inf = Tensor::new(f32::NEG_INFINITY, vb.device())?.to_dtype(vb.dtype())?;
        let tanh_gate_alpha = match ca {
            CrossAttention::Gated => Some(vb.get((1, 1, 1), "tanh_gate.alpha")?.tanh()?),
            CrossAttention::Normal => None,
        };
        Ok(Self {
            in_proj_q,
            in_proj_k,
            in_proj_v,
            out_proj,
            kv_repeat: cfg.kv_repeat,
            num_heads: cfg.num_heads,
            neg_inf,
            tanh_gate_alpha,
            span: tracing::span!(tracing::Level::TRACE, "mhca"),
        })
    }

    pub fn forward(&self, xs: &Tensor, ca_src: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let _enter = self.span.enter();
        if self.kv_repeat != 1 {
            candle::bail!("only kv-repeat = 1 is supported")
        }
        let (b, t, hd) = xs.dims3()?;
        let head_dim = hd / self.num_heads;
        // time_dim = 1, layout: b,t,h,d
        let q = xs.apply(&self.in_proj_q)?;
        let k = ca_src.apply(&self.in_proj_k)?;
        let v = ca_src.apply(&self.in_proj_v)?;
        let (ca_b, ca_t, ca_dim) = k.dims3()?;
        let q = q.reshape((b, t, self.num_heads, head_dim))?;
        let k = k.reshape((ca_b, ca_t, ca_dim / head_dim, head_dim))?;
        let v = v.reshape((ca_b, ca_t, ca_dim / head_dim, head_dim))?;
        // qk_layer_norm = None
        // kv_repeat = 1, otherwise we would need repeat_kv
        let q = q.transpose(1, 2)?.contiguous()?; // b,h,t,d
        let k = k.transpose(1, 2)?.contiguous()?; // b,h,k,d
        let v = v.transpose(1, 2)?.contiguous()?; // b,h,k,d

        let pre_ws = q.matmul(&k.t()?)?; // b,h,t,k
        let pre_ws = (pre_ws * (head_dim as f64).powf(-0.5))?;

        let pre_ws = match mask {
            None => pre_ws,
            Some(mask) => {
                let mask = mask.broadcast_left((b, self.num_heads))?;
                let neg_inf = self.neg_inf.broadcast_as(pre_ws.shape())?;
                mask.where_cond(&neg_inf, &pre_ws)?
            }
        };

        let ws = candle_nn::ops::softmax_last_dim(&pre_ws)?; // b,h,t,k
        let xs = ws.matmul(&v)?; // b,h,t,d
        let xs = xs
            .transpose(1, 2)? // b,t,h,d
            .reshape((b, t, hd))?
            .apply(&self.out_proj)?;
        let xs = match self.tanh_gate_alpha.as_ref() {
            None => xs,
            Some(alpha) => xs.broadcast_mul(alpha)?,
        };
        Ok(xs)
    }
}

#[derive(Debug, Clone)]
pub enum Mlp {
    NoGating { linear1: Linear, linear2: Linear },
    Gating { linear_in: Linear, linear_out: Linear, activation: candle_nn::Activation },
}

impl Mlp {
    pub fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let d_model = cfg.d_model;
        match cfg.gating {
            None => {
                let linear1 = linear(d_model, cfg.dim_feedforward, cfg.bias_ff, vb.pp("linear1"))?;
                let linear2 = linear(cfg.dim_feedforward, d_model, cfg.bias_ff, vb.pp("linear2"))?;
                Ok(Self::NoGating { linear1, linear2 })
            }
            Some(activation) => {
                let vb = vb.pp("gating");
                let hidden = if cfg.dim_feedforward == 4 * d_model {
                    11 * d_model / 4
                } else {
                    2 * cfg.dim_feedforward / 3
                };
                // TODO: Maybe use bias_ff here?
                let linear_in = linear(d_model, 2 * hidden, false, vb.pp("linear_in"))?;
                let linear_out = linear(hidden, d_model, false, vb.pp("linear_out"))?;
                Ok(Self::Gating { linear_in, linear_out, activation })
            }
        }
    }
}

impl Module for Mlp {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        match self {
            Self::NoGating { linear1, linear2 } => xs.apply(linear1)?.gelu_erf()?.apply(linear2),
            Self::Gating { linear_in, linear_out, activation } => {
                let xs = xs.apply(linear_in)?;
                let (b, t, _) = xs.dims3()?;
                let xs = xs.reshape((b, t, 2, ()))?;
                let xs = (xs.i((.., .., 0))?.apply(activation)? * xs.i((.., .., 1))?)?;
                xs.apply(linear_out)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RmsNorm {
    pub(crate) alpha: Tensor,
    pub(crate) eps: f32,
}

impl RmsNorm {
    pub fn new(d_model: usize, eps: f32, vb: VarBuilder) -> Result<Self> {
        let alpha = vb.get((1, 1, d_model), "alpha")?.reshape(d_model)?;
        Ok(Self { alpha, eps })
    }
}

impl Module for RmsNorm {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        candle_nn::ops::rms_norm(xs, &self.alpha, self.eps)
    }
}

#[derive(Debug, Clone)]
pub struct LayerNorm {
    inner: candle_nn::LayerNorm,
}

impl LayerNorm {
    pub fn new(d_model: usize, eps: f32, vb: VarBuilder) -> Result<Self> {
        let bias = vb.get(d_model, "bias")?;
        let alpha = if vb.contains_tensor("alpha") {
            vb.get((1, 1, d_model), "alpha")?.reshape(d_model)?
        } else {
            vb.get(d_model, "weight")?.reshape(d_model)?
        };
        let inner = candle_nn::LayerNorm::new(alpha, bias, eps as f64);
        Ok(Self { inner })
    }

    pub fn new_no_bias(d_model: usize, eps: f32, vb: VarBuilder) -> Result<Self> {
        let alpha = if vb.contains_tensor("alpha") {
            vb.get((1, 1, d_model), "alpha")?.reshape(d_model)?
        } else {
            vb.get(d_model, "weight")?.reshape(d_model)?
        };
        let inner = candle_nn::LayerNorm::new_no_bias(alpha, eps as f64);
        Ok(Self { inner })
    }
}

impl Module for LayerNorm {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        self.inner.forward(xs)
    }
}

#[derive(Debug, Clone)]
pub enum Norm {
    LayerNorm(LayerNorm),
    RmsNorm(RmsNorm),
}

impl Norm {
    pub fn new(d_model: usize, cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let norm = match cfg.norm {
            crate::NormType::LayerNorm => {
                let norm = LayerNorm::new(d_model, 1e-5, vb)?;
                Self::LayerNorm(norm)
            }
            crate::NormType::RmsNorm => {
                let norm = RmsNorm::new(d_model, 1e-8, vb)?;
                Self::RmsNorm(norm)
            }
        };
        Ok(norm)
    }
}

impl Module for Norm {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        match self {
            Self::LayerNorm(m) => m.forward(xs),
            Self::RmsNorm(m) => m.forward(xs),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamingTransformerLayer {
    self_attn: StreamingMultiheadAttention,
    mlp: Mlp,
    norm1: Norm,
    norm2: Norm,
    layer_scale_1: Option<LayerScale>,
    layer_scale_2: Option<LayerScale>,
    cross_attn: Option<(LayerNorm, StreamingMultiheadCrossAttention)>,
    norm_first: bool,
    span: tracing::Span,
}

impl StreamingTransformerLayer {
    pub fn new(rope: &Option<Arc<RotaryEmbedding>>, cfg: &Config, vb: VarBuilder) -> Result<Self> {
        if cfg.use_conv_block {
            candle::bail!("conv-block is not supported")
        }
        let d_model = cfg.d_model;
        let mlp = Mlp::new(cfg, vb.clone())?;
        let (norm1, norm2) = match cfg.norm {
            crate::NormType::LayerNorm => {
                let norm1 = LayerNorm::new(d_model, 1e-5, vb.pp("norm1"))?;
                let norm2 = LayerNorm::new(d_model, 1e-5, vb.pp("norm2"))?;
                (Norm::LayerNorm(norm1), Norm::LayerNorm(norm2))
            }
            crate::NormType::RmsNorm => {
                let norm1 = RmsNorm::new(d_model, 1e-8, vb.pp("norm1"))?;
                let norm2 = RmsNorm::new(d_model, 1e-8, vb.pp("norm2"))?;
                (Norm::RmsNorm(norm1), Norm::RmsNorm(norm2))
            }
        };
        let layer_scale_1 = match cfg.layer_scale {
            None => None,
            Some(ls) => {
                let ls = LayerScale::new(d_model, ls, vb.pp("layer_scale_1"))?;
                Some(ls)
            }
        };
        let layer_scale_2 = match cfg.layer_scale {
            None => None,
            Some(ls) => {
                let ls = LayerScale::new(d_model, ls, vb.pp("layer_scale_2"))?;
                Some(ls)
            }
        };
        let self_attn = StreamingMultiheadAttention::new(rope, cfg, vb.pp("self_attn"))?;
        let cross_attn = match cfg.cross_attention {
            Some(ca) => {
                let norm_cross = LayerNorm::new_no_bias(cfg.d_model, 1e-5, vb.pp("norm_cross"))?;
                let cross_attn =
                    StreamingMultiheadCrossAttention::new(ca, cfg, vb.pp("cross_attention"))?;
                Some((norm_cross, cross_attn))
            }
            None => None,
        };
        Ok(Self {
            self_attn,
            mlp,
            norm1,
            norm2,
            layer_scale_1,
            layer_scale_2,
            cross_attn,
            norm_first: cfg.norm_first,
            span: tracing::span!(tracing::Level::TRACE, "transformer-layer"),
        })
    }

    pub fn forward(
        &mut self,
        xs: &Tensor,
        ca_src: Option<&Tensor>,
        mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let _enter = self.span.enter();
        if !self.norm_first {
            candle::bail!("only norm_first = true is supported")
        }
        let norm1 = xs.apply(&self.norm1)?;
        let xs =
            (xs + self.self_attn.forward(&norm1, mask)?.apply(&self.layer_scale_1.as_ref())?)?;

        let xs = match (&self.cross_attn, ca_src) {
            (Some((norm_cross, cross_attn)), Some(ca_src)) => {
                let residual = &xs;
                let xs = xs.apply(norm_cross)?;
                (residual + cross_attn.forward(&xs, ca_src, None)?)?
            }
            _ => xs,
        };

        let xs =
            (&xs + xs.apply(&self.norm2)?.apply(&self.mlp)?.apply(&self.layer_scale_2.as_ref()))?;
        Ok(xs)
    }

    pub fn reset_kv_cache(&mut self) {
        self.self_attn.reset_kv_cache()
    }

    pub fn set_kv_cache(&mut self, kv_cache: candle_nn::kv_cache::KvCache) {
        self.self_attn.set_kv_cache(kv_cache)
    }
}

#[derive(Debug, Clone)]
pub struct StreamingTransformer {
    layers: Vec<StreamingTransformerLayer>,
    context: usize,
    positional_embedding: PositionalEmbedding,
    max_period: usize,
}

impl StreamingTransformer {
    pub fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let vb_l = vb.pp("layers");
        let rope = match cfg.positional_embedding {
            PositionalEmbedding::Rope => {
                let rope = RotaryEmbedding::new(
                    cfg.d_model / cfg.num_heads,
                    cfg.max_seq_len,
                    cfg.max_period as f32,
                    vb.device(),
                )?;
                Some(Arc::new(rope))
            }
            PositionalEmbedding::Sin | PositionalEmbedding::None => None,
        };
        let mut layers = Vec::with_capacity(cfg.num_layers);
        for layer_idx in 0..cfg.num_layers {
            let layer = StreamingTransformerLayer::new(&rope, cfg, vb_l.pp(layer_idx))?;
            layers.push(layer)
        }
        Ok(Self {
            layers,
            context: cfg.context,
            positional_embedding: cfg.positional_embedding,
            max_period: cfg.max_period,
        })
    }

    pub fn forward(&mut self, xs: &Tensor) -> Result<Tensor> {
        self.forward_ca(xs, None)
    }

    pub fn forward_ca(&mut self, xs: &Tensor, ca_src: Option<&Tensor>) -> Result<Tensor> {
        let (_b, t, c) = xs.dims3()?;
        // We will extract at most "context" from the kv_cache.
        // Note that the mask will discard the values that are before context.
        let pos = self.layers[0].self_attn.kv_cache.k_cache().current_seq_len().min(self.context);
        let mask =
            if t == 1 { None } else { Some(get_mask(t, pos + t, self.context, xs.device())?) };
        let mut xs = match self.positional_embedding {
            PositionalEmbedding::Rope | PositionalEmbedding::None => xs.clone(),
            PositionalEmbedding::Sin => {
                let dev = xs.device();
                let theta = self.max_period as f32;
                let half_dim = c / 2;
                let positions = Tensor::arange(pos as u32, (pos + t) as u32, dev)?
                    .unsqueeze(1)?
                    .to_dtype(DType::F32)?;
                let inv_freq: Vec<_> = (0..half_dim)
                    .map(|i| 1f32 / theta.powf(i as f32 / (half_dim - 1) as f32))
                    .collect();
                let inv_freq_len = inv_freq.len();
                let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?;
                let freqs = positions.broadcast_mul(&inv_freq)?;
                let pos_emb =
                    Tensor::cat(&[freqs.cos()?, freqs.sin()?], D::Minus1)?.to_dtype(xs.dtype())?;
                xs.broadcast_add(&pos_emb)?
            }
        };
        for layer in self.layers.iter_mut() {
            xs = layer.forward(&xs, ca_src, mask.as_ref())?;
        }
        Ok(xs)
    }

    pub fn copy_state(&mut self, from: &Self) -> Result<()> {
        if self.layers.len() != from.layers.len() {
            candle::bail!("cannot copy kv-caches as the transformers have different depths")
        }
        self.layers
            .iter_mut()
            .zip(from.layers.iter())
            .for_each(|(v, w)| v.set_kv_cache(w.self_attn.kv_cache.clone()));
        Ok(())
    }
}

impl StreamingModule for StreamingTransformer {
    fn reset_state(&mut self) {
        self.layers.iter_mut().for_each(|v| v.reset_kv_cache())
    }

    fn step(&mut self, xs: &StreamTensor) -> Result<StreamTensor> {
        match xs.as_option() {
            None => Ok(StreamTensor::empty()),
            Some(xs) => Ok(StreamTensor::from_tensor(self.forward(xs)?)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectedTransformer {
    transformer: StreamingTransformer,
    input_proj: Option<Linear>,
    output_projs: Vec<Option<Linear>>,
    conv_layout: bool,
    span: tracing::Span,
}

impl ProjectedTransformer {
    pub fn new(
        input_dim: usize,
        output_dims: &[usize],
        cfg: &Config,
        vb: VarBuilder,
    ) -> Result<Self> {
        let transformer = StreamingTransformer::new(cfg, vb.pp("transformer"))?;
        let input_proj = if input_dim == cfg.d_model {
            None
        } else {
            let l = linear_no_bias(input_dim, cfg.d_model, vb.pp("input_proj"))?;
            Some(l)
        };
        let mut output_projs = Vec::with_capacity(output_dims.len());
        let vb_o = vb.pp("output_projs");
        for (i, &output_dim) in output_dims.iter().enumerate() {
            let output_proj = if output_dim == cfg.d_model {
                None
            } else {
                let l = linear_no_bias(cfg.d_model, output_dim, vb_o.pp(i))?;
                Some(l)
            };
            output_projs.push(output_proj)
        }
        Ok(Self {
            transformer,
            input_proj,
            output_projs,
            conv_layout: cfg.conv_layout,
            span: tracing::span!(tracing::Level::TRACE, "proj-transformer"),
        })
    }

    pub fn forward(&mut self, xs: &Tensor) -> Result<Vec<Tensor>> {
        let _enter = self.span.enter();
        let xs = if self.conv_layout { xs.transpose(1, 2)? } else { xs.clone() };
        let xs = xs.apply(&self.input_proj.as_ref())?;
        let xs = self.transformer.forward(&xs)?;
        let mut ys = Vec::with_capacity(self.output_projs.len());
        for output_proj in self.output_projs.iter() {
            let ys_ = xs.apply(&output_proj.as_ref())?;
            let ys_ = if self.conv_layout { ys_.transpose(1, 2)? } else { ys_ };
            ys.push(ys_)
        }
        Ok(ys)
    }
}

impl StreamingModule for ProjectedTransformer {
    fn reset_state(&mut self) {
        self.transformer.reset_state()
    }

    fn step(&mut self, xs: &StreamTensor) -> Result<StreamTensor> {
        let xs = xs.apply(&|x: &Tensor| {
            if self.conv_layout {
                x.transpose(1, 2)
            } else {
                Ok(x.clone())
            }
        })?;
        let xs = xs.apply(&self.input_proj.as_ref())?;
        let xs = self.transformer.step(&xs)?;
        let ys = xs.apply(&self.output_projs[0].as_ref())?;
        ys.apply(&|y: &Tensor| {
            if self.conv_layout {
                y.transpose(1, 2)
            } else {
                Ok(y.clone())
            }
        })
    }
}

#[cfg(feature = "flash-attn")]
fn flash_attn(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    softmax_scale: f32,
    causal: bool,
) -> Result<Tensor> {
    candle_flash_attn::flash_attn(q, k, v, softmax_scale, causal)
}

#[cfg(not(feature = "flash-attn"))]
fn flash_attn(_: &Tensor, _: &Tensor, _: &Tensor, _: f32, _: bool) -> Result<Tensor> {
    unimplemented!("compile with '--features flash-attn'")
}
