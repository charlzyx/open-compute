ARG BASE_IMAGE=debian:13-slim
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends adduser systemd systemd-sysv dbus-user-session libpam-systemd sudo curl ca-certificates python3 procps && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --shell /bin/bash installtest && printf 'installtest ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/installtest && chmod 0440 /etc/sudoers.d/installtest
# The fixture shares its host kernel and must not unregister host binfmt handlers.
RUN ln -s /dev/null /etc/systemd/system/systemd-binfmt.service
ENV container=docker
CMD ["/sbin/init"]
