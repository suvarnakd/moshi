#FROM nvidia/cuda:12.6.1-devel-ubi9
FROM nvidia/cuda:12.6.1-devel-ubuntu22.04
# FROM mcr.microsoft.com/devcontainers/base:jammy 
FROM ubuntu
ARG DEBIAN_FRONTEND=noninteractive
RUN mkdir -p /home/suvarna

# Set the home directory for the user
ENV HOME="/home/suvarna"
COPY . ${HOME}
# Create a work directory
WORKDIR ${HOME}/moshi

RUN apt update -y \ 
    && apt install -y build-essential --no-install-recommends make \
        ca-certificates \
        git \
        libssl-dev \
        zlib1g-dev \
        libbz2-dev \
        libreadline-dev \
        libsqlite3-dev \
        wget \
        curl \
        llvm \
        libncurses5-dev \
        xz-utils \
        tk-dev \
        libxml2-dev \
        libxmlsec1-dev \
        libffi-dev \
        liblzma-dev \
        python3 \
        python3-pip  \
        python3-venv 

ENV VIRTUAL_ENV=/opt/venv
RUN python3 -m venv $VIRTUAL_ENV
ENV PATH="$VIRTUAL_ENV/bin:${HOME}:$PATH"
    
# Install dependencies:
#RUN pip install -r ${HOME}/moshi/moshi/requirements.txt
# RUN pip install -r /workspaces/*/moshi/requirements.txt
# Expose a port for your application (if needed)
EXPOSE 8998

# Define the command to run your application
CMD [ "bash" ]
# CMD ["python3", "-m", "moshi.server --gradio-tunnel --hf-repo kyutai/moshika-pytorch-bf16"]