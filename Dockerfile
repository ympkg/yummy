FROM eclipse-temurin:21-jdk AS base

LABEL org.opencontainers.image.source="https://github.com/ympkg/yummy"
LABEL org.opencontainers.image.description="Yummy (ym) - Modern Java build tool"

COPY ym /usr/local/bin/ym
COPY ym /usr/local/bin/ymc
COPY ym-agent.jar /usr/local/lib/ym-agent.jar

WORKDIR /workspace

ENTRYPOINT ["ym"]
