FROM debian:bookworm-slim

ARG PACKER_VERSION=1.11.2
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    cloud-image-utils \
    curl \
    genisoimage \
    jq \
    openssh-client \
    qemu-system-x86 \
    qemu-utils \
    unzip \
    xorriso \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL -o /tmp/packer.zip "https://releases.hashicorp.com/packer/${PACKER_VERSION}/packer_${PACKER_VERSION}_linux_amd64.zip" \
 && unzip /tmp/packer.zip -d /usr/local/bin \
 && rm /tmp/packer.zip

CMD ["/bin/bash"]
