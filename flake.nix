{
  description = "Conversations — development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/46db2e09e1d3f113a13c0d7b81e2f221c63b8ce9";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachSystem [
      "aarch64-darwin"
       "aarch64-linux"
       "x86_64-linux"
       "x86_64-darwin"
    ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Use the rust-toolchain.toml release, as CI and the production image do.
        # Override replaces the file's components, so list them here too, plus
        # the editor sources.
        rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [ "rustfmt" "clippy" "rust-src" "rust-analyzer" ];
        };

        # Platform-specific native deps (Linux only; on macOS the system SDK is used automatically)
        nativeDeps = pkgs.lib.optionals pkgs.stdenv.isLinux [
          pkgs.openssl
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ pkgs.pkg-config ];

          buildInputs = [
            # --- Rust ---
            rustToolchain

            # --- Node.js (React / Vite / TypeScript frontend) ---
            pkgs.nodejs_22
            pkgs.python3 # isolated cmux/native-agent test fixtures

            # --- Database tooling (swappable SQLite / Postgres via sqlx) ---
            pkgs.postgresql_17    # psql client + libs for sqlx Postgres / local pg
            pkgs.sqlite

            # --- Kubernetes / kind deploy ---
            pkgs.kubectl
            pkgs.kind
            pkgs.kubernetes-helm
            pkgs.k9s
            pkgs.docker-client    # CLI only — daemon runs elsewhere

            # --- LSPs ---
            pkgs.nil                                          # Nix
            pkgs.nodePackages.typescript-language-server      # TypeScript / JS
            pkgs.yaml-language-server                         # YAML / K8s manifests

            # --- Utilities ---
            pkgs.jq
            pkgs.git
            pkgs.prek
          ] ++ nativeDeps;

          # Ensure openssl-sys finds the right library on Linux
          # (reqwest uses rustls here, but keep this for portability/safety)
          env = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            OPENSSL_DIR = "${pkgs.openssl.dev}";
            OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          };

          shellHook = ''
            export PROJECT_ROOT="$PWD"
            echo "Conversations dev shell"
          '';
        };
      }
    );
}
