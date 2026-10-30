# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 Meros Inc.

ARG BUILD_FROM=alpine:3.22

FROM rust:1-alpine3.22 AS build
RUN apk add --no-cache musl-dev git
WORKDIR /src
COPY vendor vendor
COPY sender sender
# Home Assistant may clone this repository without its submodule: fetch
# libaes67 at the pinned commit when it is missing.
ARG AES67_COMMIT=b2562d6b453d8c08e167eb5fa6c78f3d697d555e
RUN if [ ! -f vendor/aes67/aes67/src/rtp.c ]; then \
        rm -rf vendor/aes67 \
        && git clone https://github.com/meros-co/aes67.git vendor/aes67 \
        && git -C vendor/aes67 checkout "$AES67_COMMIT"; \
    fi
RUN cd sender && cargo build --release --locked && strip target/release/aes67-sender

# Alpine has no linuxptp package: build ptp4l from the pinned upstream release.
FROM alpine:3.22 AS linuxptp
RUN apk add --no-cache build-base linux-headers bsd-compat-headers git
ARG LINUXPTP_COMMIT=0188667308e62b6ac61151193320ec052e2fd023
RUN git clone https://github.com/richardcochran/linuxptp.git /linuxptp     && cd /linuxptp && git checkout "$LINUXPTP_COMMIT"     && make ptp4l pmc && strip ptp4l pmc

FROM ${BUILD_FROM}
COPY --from=linuxptp /linuxptp/ptp4l /linuxptp/pmc /usr/sbin/
COPY --from=build /src/sender/target/release/aes67-sender /usr/bin/aes67-sender
CMD ["/usr/bin/aes67-sender", "/data/options.json"]
