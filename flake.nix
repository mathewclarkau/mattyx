{
  description = "mattyx (mtyx): agent-aware terminal multiplexer backed by libghostty-vt";

  inputs = {
    # Pinned to a specific nixos-unstable commit (2026-09-10) whose default
    # Rust is 1.97 (`rust = rust_1_97`, matching the AGENTS.md toolchain law
    # and the dtolnay/rust-toolchain@1.97.0 pin in release.yml). nixpkgs
    # keeps only one numbered Rust attribute at a time, so when bumping this
    # rev, re-check which `rust_1_XX` it ships and update the rustPlatform
    # pin in mkMattyx accordingly. A generated flake.lock is still to be
    # committed on a machine with nix available (deferred — see fix-notes).
    nixpkgs.url = "github:NixOS/nixpkgs/317f4cac2bc14319572ce85ca7016317758c695a";
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
          # NOTE (sandbox limitation, issue #141 review F5): upstream
          # ghostty/build.zig.zon.nix fetches the zig packages with
          # `zig fetch` inside a non-fixed-output derivation
          # (runCommandLocal), so a stock sandboxed `nix build` denies it
          # network and the fetch fails. Until the dependencies are
          # pre-fetched as fixed-output derivations (deferred: that needs
          # FOD hashes for every entry in ghostty/build.zig.zon, and the
          # pinned ghostty submodule must not be edited), build with
          #   nix build --option sandbox false .#mtyx
          # (documented in README.md next to the nix instructions).
          ghosttyZigDeps = pkgs.callPackage ./ghostty/build.zig.zon.nix {
            inherit zig_0_15;
            name = "mattyx-ghostty-zig-deps";
          };
          # Version comes from the release tag only (AGENTS.md / issue
          # #71) — never from the stale Cargo.toml manifest floor, and
          # never synthesized. The intended source of this value is a
          # real `git describe --tags --match 'v*'` of the tree being
          # built (whose output may legitimately carry a `-g<sha>`
          # suffix; this file never invents one). Flakes evaluate purely
          # by default, where environment variables are invisible and
          # the flake source carries no .git, so the tag cannot be
          # discovered from inside the flake: pass --impure and set
          # MTYX_VERSION. Anything else fails loudly rather than
          # building a mislabeled binary (e.g. a v0.27.0 tree reporting
          # 0.17.2-g<sha>).
          mtyxVersion =
            let
              raw = builtins.getEnv "MTYX_VERSION";
              stripped = if lib.hasPrefix "v" raw then builtins.substring 1 (-1) raw else raw;
            in
            if raw == "" then
              builtins.abort ''
                mattyx flake: cannot determine the mtyx version.
                The version must come from the release tag (issue #71), never
                from the Cargo.toml manifest floor, and the flake cannot
                discover the tag on its own under pure evaluation. Set
                MTYX_VERSION to the version being built — typically from a
                real describe of the tree:
                    MTYX_VERSION="$(git describe --tags --match 'v*')" nix build --impure .#mtyx
                (a leading "v" is stripped; --impure is required so the
                flake can read the environment).''
            else if builtins.match "[0-9]+\\.[0-9]+\\.[0-9]+.*" stripped == null then
              builtins.abort ''
                mattyx flake: MTYX_VERSION '${raw}' does not look like a
                release version (expected X.Y.Z, optionally with a real
                git-describe suffix such as X.Y.Z-3-g1a2b3c4).''
            else stripped;
          # Rust 1.97 per AGENTS.md — explicit instead of floating with
          # whatever rust the nixpkgs rev happens to ship; matches the
          # dtolnay/rust-toolchain@1.97.0 pin in release.yml. The
          # nixpkgs input above is pinned to a rev whose default rust is
          # 1.97; rustPackages_1_97 keeps the pin visible and survives an
          # input bump (update it together with the rev).
          rustPlatform = pkgs.rustPackages_1_97.rustPlatform;
        in
        rustPlatform.buildRustPackage {
          pname = "mtyx";
          version = mtyxVersion;

          # Standard idiom: name the copied source tree so sourceRoot can
          # point inside it. (The previous sourceRoot derived from
          # self.narHash — a content digest, not a directory name — so
          # unpackPhase looked for a directory that never exists.)
          src = builtins.path { path = ./.; name = "source"; };
          sourceRoot = "source/mux";

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
          MTYX_VERSION = mtyxVersion;

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
