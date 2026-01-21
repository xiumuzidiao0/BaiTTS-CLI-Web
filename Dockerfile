# ---- Stage 1: Build ----
# Start from the same base as the runtime to ensure glibc compatibility
FROM debian:12-slim AS builder

# Install build dependencies, curl for rustup, and ca-certificates for networking
RUN apt-get update && apt-get install -y --no-install-recommends curl build-essential ca-certificates pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Install rustup and the nightly toolchain
# The -y flag automates the installation. --default-toolchain nightly sets our required compiler.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly

# Add cargo to the shell's PATH environment variable for subsequent commands
ENV PATH="/root/.cargo/bin:${PATH}"

# Set the working directory
WORKDIR /usr/src/app

# Copy the entire project context (respecting .dockerignore)
COPY . .

# Build the application in release mode using the nightly compiler
RUN cargo build --release
RUN strip /usr/src/app/target/release/baitts-cli-rs

# ---- Stage 2: Runtime ----
# Use the same slim, secure base image for the final container.
FROM debian:12-slim

# Install libssl, which is a required runtime dependency for reqwest (used for HTTPS).
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 && rm -rf /var/lib/apt/lists/*

# Set the working directory for the runtime environment.
WORKDIR /usr/local/bin

# Copy the compiled binary from the 'builder' stage.
COPY --from=builder /usr/src/app/target/release/baitts-cli-rs .

# Copy the entrypoint script and make it executable
COPY docker-entrypoint.sh .
RUN chmod +x ./docker-entrypoint.sh

# Declare volumes for input books and audio output
VOLUME /book
VOLUME /output
VOLUME /data

# Expose the port the web server listens on.
EXPOSE 5688

# Environment variable for auto-run
ENV AUTORUN=false

# Set the entrypoint to our script
ENTRYPOINT ["./docker-entrypoint.sh"]

# Set the default command to run when the container starts.
CMD ["./baitts-cli-rs", "--web"]