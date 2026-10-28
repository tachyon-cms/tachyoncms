# The release image, built by .github/workflows/release.yml from the static
# binaries it has already tested.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab
ARG TARGETPLATFORM
COPY $TARGETPLATFORM/tachyon /usr/local/bin/tachyon
# Listening beyond the container also needs TACHYON_PUBLIC_URL, and the
# database needs TACHYON_DATABASE_URL.
ENV TACHYON_LISTEN=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/tachyon"]
CMD ["serve"]
