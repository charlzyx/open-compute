ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG REVISION
RUN command -v python3
ENV IMAGE_REVISION=${REVISION}
COPY serve.sh /serve.sh
CMD ["/bin/sh", "/serve.sh"]
