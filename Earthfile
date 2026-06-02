VERSION 0.8

packer-tools-image:
    FROM DOCKERFILE --platform=linux/amd64 -f containers/packer-tools/Dockerfile containers/packer-tools
    SAVE IMAGE botwork/packer-tools:local

images:
    BUILD +packer-tools-image
