FROM rust:latest

# Install mingw-w64 for Windows GNU cross-compilation
RUN apt-get update \
 && apt-get install -y --no-install-recommends g++-mingw-w64-x86-64 \
 && rm -rf /var/lib/apt/lists/*

# Add the Windows GNU target
RUN rustup target add x86_64-pc-windows-gnu

WORKDIR /app

# Build when the container runs (expects your project to be mounted/copied into /app)
CMD ["cargo", "build", "--release",  "--target", "x86_64-pc-windows-gnu"]
