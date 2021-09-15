FROM quay.io/coreos-assembler/fcos-buildroot:testing-devel as builder
WORKDIR /src
COPY . .
RUN make && make install DESTDIR=/build

FROM quay.io/coreos-assembler/coreos-assembler:latest
WORKDIR /srv
USER root
# Copy binaries from the build
COPY --from=builder /build /build
# Copy the build script
COPY --from=builder /src/ci/prow/fcos-e2e.sh /usr/bin/fcos-e2e
RUN /usr/bin/fcos-e2e
