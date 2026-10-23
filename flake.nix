{
  description = "mattyx (mtyx): agent-aware terminal multiplexer backed by libghostty-vt";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAllSystems = lib.genAttrs supportedSystems;

      # Pinned Zig 0.15.2 binary distribution per AGENTS.md (enforced at
      # compile time by ghostty/build.zig via @compileError).
      zigSources = {
        "x86_64-linux" = {
          url = "https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz";
          sha256 = "02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239";
        };
        "aarch64-linux" = {
          url = "https://ziglang.org/download/0.15.2/zig-aarch64-linux-0.15.2.tar.xz";
          sha256 = "958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f";
        };
        "aarch64-darwin" = {
          url = "https://ziglang.org/download/0.15.2/zig-aarch64-macos-0.15.2.tar.xz";
          sha256 = "3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b";
        };
        "x86_64-darwin" = {
          url = "https://ziglang.org/download/0.15.2/zig-x86_64-macos-0.15.2.tar.xz";
          sha256 = "375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f";
        };
      };

      mkZig_0_15_2 = pkgs:
        let
          meta = zigSources.${pkgs.stdenv.hostPlatform.system};
        in
        pkgs.stdenvNoCC.mkDerivation {
          pname = "zig";
          version = "0.15.2";
          src = pkgs.fetchurl {
            inherit (meta) url sha256;
          };
          dontConfigure = true;
          dontBuild = true;
          installPhase = ''
            mkdir -p "$out/bin" "$out/lib"
            cp -r zig "$out/bin/zig"
            cp -r lib/* "$out/lib/"
            chmod +x "$out/bin/zig"
          '';
        };

      mkMattyx = pkgs:
        let
          zig_0_15 = mkZig_0_15_2 pkgs;
          ghosttyZigDeps = pkgs.callPackage ./ghostty/build.zig.zon.nix {
            inherit zig_0_15;
            name = "mattyx-ghostty-zig-deps";
          };
          cargoToml = builtins.fromTOML (builtins.readFile ./mux/Cargo.toml);
          revSuffix =
            if (self ? shortRev) then "-g${self.shortRev}"
            else if (self ? dirtyShortRev) then "-g${self.dirtyShortRev}"
            else "";
          version = "${cargoToml.workspace.package.version}${revSuffix}";
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "mtyx";
          inherit version;

          src = self;
          sourceRoot = "${self.narHash or "source"}/mux";

          cargoLock = {
            lockFile = ./mux/Cargo.lock;
          };

          buildAndTestSubdir = "crates/mux-tui";

          nativeBuildInputs = [
            zig_0_15
            pkgs.llvmPackages.clang
            pkgs.llvmPackages.libclang
            pkgs.git
          ];

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          ZIG = "${zig_0_15}/bin/zig";
          MTYX_GHOSTTY_VT_ZIG_CPU = "baseline";
          MTYX_GHOSTTY_VT_ZIG_SYSTEM = "${ghosttyZigDeps}";
          MTYX_VERSION = version;

          preBuild = ''
            export HOME="$TMPDIR"
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            mkdir -p "$ZIG_GLOBAL_CACHE_DIR"

            # Apply repo patches to a writable copy of the ghostty submodule.
            cp -r ../ghostty "$TMPDIR/ghostty"
            chmod -R u+w "$TMPDIR/ghostty"
            for patch in ../patches/*.patch; do
              [ -e "$patch" ] || continue
              patch -d "$TMPDIR/ghostty" -p1 --forward < "$patch" || true
            done
            export MTYX_GHOSTTY_SRC="$TMPDIR/ghostty"
          '';

          # Integration tests spawn PTYs/sockets that are tested in CI outside
          # the restricted Nix build sandbox.
          doCheck = false;

          meta = with lib; {
            description = "Agent-aware terminal multiplexer backed by libghostty-vt";
            homepage = "https://github.com/mathewclarkau/mattyx";
            license = licenses.gpl3Plus;
            mainProgram = "mtyx";
            platforms = supportedSystems;
          };
        };
    in
    {
      overlays.default = final: prev: {
        mtyx = mkMattyx final;
        mattyx = final.mtyx;
      };

      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          pkg = mkMattyx pkgs;
        in
        {
          default = pkg;
          mtyx = pkg;
          mattyx = pkg;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.mtyx}/bin/mtyx";
        };
        mtyx = {
          type = "app";
          program = "${self.packages.${system}.mtyx}/bin/mtyx";
        };
      });
    };
}
