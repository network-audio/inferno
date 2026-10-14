FROM inferno_aoip:fedora-alsa
RUN dnf install -y pipewire wireplumber pipewire-libs pipewire-utils sox
WORKDIR /app
ENV XDG_RUNTIME_DIR=/tmp
# the PipeWire nodes as users install them (install.sh --pipewire)
RUN install -D -m 644 /opt/inferno/os_integration/pipewire/50-inferno.conf /etc/pipewire/pipewire.conf.d/50-inferno.conf
ADD pipewire_duplex.sh /app
CMD [ "/app/pipewire_duplex.sh" ]
