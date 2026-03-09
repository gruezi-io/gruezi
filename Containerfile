FROM ubuntu:24.04

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates iproute2 iputils-arping ndisc6 \
  && rm -rf /var/lib/apt/lists/*

COPY target/debug/gruezi /usr/local/bin/gruezi

ENTRYPOINT ["/usr/local/bin/gruezi"]
