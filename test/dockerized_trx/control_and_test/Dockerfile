# glibc: netaudio's prebuilt (manylinux) wheels install as they are. It has no musl wheels, and its Rust core
# doesn't build against musl (netaudio 0.5.6)
FROM python:3.14-slim-trixie

RUN apt-get update && apt-get install --yes --no-install-recommends git build-essential sox \
    && rm -rf /var/lib/apt/lists/*
RUN cd / && git clone https://github.com/teodly/diff_tools && cd diff_tools && ./build_linux.sh

# 0.5 and later tell devices sharing an IP apart (aplayrec3)
RUN pip install --no-cache-dir "netaudio>=0.5"
ADD control_and_test.sh /
CMD [ "/control_and_test.sh" ]
