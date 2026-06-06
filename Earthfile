VERSION 0.8

shasset-image:
    ARG BINARY_SOURCE=source
    ARG TAG=latest
    FROM DOCKERFILE --platform=linux/amd64 --build-arg BINARY_SOURCE=$BINARY_SOURCE -f shasset/Dockerfile .
    SAVE IMAGE botwork/shasset:local
    IF [ "$TAG" = "latest" ]
        SAVE IMAGE --push ghcr.io/botworkz/tools/shasset:latest
    ELSE
        SAVE IMAGE --push ghcr.io/botworkz/tools/shasset:${TAG}
        SAVE IMAGE --push ghcr.io/botworkz/tools/shasset:latest
    END

botforge-image:
    ARG BINARY_SOURCE=source
    ARG TAG=latest
    FROM DOCKERFILE --platform=linux/amd64 --build-arg BINARY_SOURCE=$BINARY_SOURCE -f botforge/Dockerfile .
    SAVE IMAGE botwork/botforge:local
    IF [ "$TAG" = "latest" ]
        SAVE IMAGE --push ghcr.io/botworkz/tools/botforge:latest
    ELSE
        SAVE IMAGE --push ghcr.io/botworkz/tools/botforge:${TAG}
        SAVE IMAGE --push ghcr.io/botworkz/tools/botforge:latest
    END

images:
    BUILD +shasset-image
    BUILD +botforge-image
