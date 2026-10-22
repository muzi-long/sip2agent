FROM rust:1.92-bookworm AS builder

# crates 默认走 rsproxy.cn(字节维护的国内镜像),防 crates.io 国内链路抖动导致拉依赖失败;
# 海外构建(如 GitHub Actions)传 --build-arg CARGO_MIRROR= 直连 crates.io
ARG CARGO_MIRROR=sparse+https://rsproxy.cn/index/
RUN if [ -n "$CARGO_MIRROR" ]; then \
      printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "%s"\n' "$CARGO_MIRROR" \
      > /usr/local/cargo/config.toml; \
    fi

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin sip2agent \
    && cp target/release/sip2agent /usr/local/bin/sip2agent

FROM debian:bookworm-slim

# ca-certificates:wss:// 连厂商(rustls 读系统根证书)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
ENV TZ=Asia/Shanghai

COPY --from=builder /usr/local/bin/sip2agent /usr/local/bin/sip2agent
WORKDIR /app
# SIP 5060 udp/tcp + RTP 端口段(与 config.toml [rtp] 保持一致;生产建议 network_mode: host)
EXPOSE 5060/udp 5060/tcp 20000-20999/udp
CMD ["sip2agent", "-c", "/app/config.toml"]
