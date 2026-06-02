VERSION 0.8

packer-tools-image:
    FROM DOCKERFILE --platform=linux/amd64 -f packer-tools/Dockerfile packer-tools
    SAVE IMAGE botwork/packer-tools:local

shasset-image:
    FROM DOCKERFILE --platform=linux/amd64 -f shasset/Dockerfile shasset
    SAVE IMAGE botwork/shasset:local

images:
    BUILD +packer-tools-image
    BUILD +shasset-image
