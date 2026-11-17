# A throwaway SSH server for tests/app_ssh.rs. User "upk" has passwordless sudo.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server sudo procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash upk \
    && echo 'upk ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/upk \
    && chmod 440 /etc/sudoers.d/upk \
    && passwd -l upk
COPY entrypoint.sh /entrypoint.sh
EXPOSE 22
ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
