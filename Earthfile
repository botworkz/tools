VERSION 0.8

packer-tools-image:
    FROM DOCKERFILE --platform=linux/amd64 -f containers/packer-tools/Dockerfile containers/packer-tools
    SAVE IMAGE botwork/packer-tools:local

shasset-image:
    ARG TAG=latest
    FROM DOCKERFILE --platform=linux/amd64 -f containers/shasset/Dockerfile .
    SAVE IMAGE botwork/shasset:local
    SAVE IMAGE --push ghcr.io/botworkz/tools/shasset:${TAG}
    SAVE IMAGE --push ghcr.io/botworkz/tools/shasset:latest

images:
    BUILD +packer-tools-image
    BUILD +shasset-image
