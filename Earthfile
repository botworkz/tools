VERSION 0.8

packer-tools-image:
    FROM DOCKERFILE --platform=linux/amd64 -f containers/packer-tools/Dockerfile containers/packer-tools
    SAVE IMAGE botwork/packer-tools:local

shasset-image:
    FROM DOCKERFILE --platform=linux/amd64 -f containers/shasset/Dockerfile .
    SAVE IMAGE botwork/shasset:local

images:
    BUILD +packer-tools-image
    BUILD +shasset-image
