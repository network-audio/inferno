FROM python:3.13-alpine3.22

# netaudio has no musl wheels: pip compiles its C dependencies and its Rust core (cargo)
RUN apk add git build-base linux-headers sox cargo
RUN cd / && git clone https://github.com/teodly/diff_tools && cd diff_tools && ./build_linux.sh

RUN CFLAGS="-Wno-int-conversion" pip install netaudio
ADD control_and_test.sh /
CMD [ "/control_and_test.sh" ]
