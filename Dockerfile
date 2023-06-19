FROM rust:1.70-buster as builder

WORKDIR /usr/src/introbot
COPY . .

RUN cargo install --path .

FROM debian:buster-slim
COPY --from=builder /usr/local/cargo/bin/introbot /usr/local/bin/introbot
CMD ["introbot"]
