VERSION 0.8

packer-tools-image:
    FROM --platform=linux/amd64 DOCKERFILE -f containers/packer-tools/Dockerfile containers/packer-tools
    SAVE IMAGE botwork/packer-tools:local

images:
    BUILD +packer-tools-image
