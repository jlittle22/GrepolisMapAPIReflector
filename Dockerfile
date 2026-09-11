FROM rust:1.83 AS application
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim AS runner
RUN apt-get update
RUN apt-get install -y ca-certificates
RUN rm -rf /var/lib/apt/lists/*
EXPOSE 3000
COPY --from=application /app/target/release/grepolis_api_reflector /grepolis_api_reflector
CMD ["/grepolis_api_reflector"]
