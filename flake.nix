{
  description = "Patchbay development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "x86_64-unknown-linux-musl" "aarch64-unknown-linux-musl" ];
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rust
            qemu
            iperf3
            cloud-utils
            zig
            cargo-zigbuild
          ];

          shellHook = ''
            echo "patchbay dev shell with qemu $(qemu-system-x86_64 --version | head -1)"
            # Create zig linker wrappers for cross-compilation
            mkdir -p "$PWD/.zig-cache/bin"

            cat > "$PWD/.zig-cache/bin/aarch64-linux-musl-zig-cc" << 'EOF'
#!/bin/sh
exec zig cc -target aarch64-linux-musl "$@"
EOF
            chmod +x "$PWD/.zig-cache/bin/aarch64-linux-musl-zig-cc"

            cat > "$PWD/.zig-cache/bin/x86_64-linux-musl-zig-cc" << 'EOF'
#!/bin/sh
exec zig cc -target x86_64-linux-musl "$@"
EOF
            chmod +x "$PWD/.zig-cache/bin/x86_64-linux-musl-zig-cc"

            export PATH="$PWD/.zig-cache/bin:$PATH"
          '';
        };
      }
    );
}
