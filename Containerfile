FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY target/debug/gruezi /usr/local/bin/gruezi

ENTRYPOINT ["/usr/local/bin/gruezi"]
