{
  description = "Super Simple Cubes - Rust voxel game (client + dedicated server)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      nixpkgsFor = system: import nixpkgs { inherit system; };

      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      # Libraries the client links and/or loads at runtime (ALSA for cpal,
      # udev for gilrs, the rest dlopen'd by the windowing/GL crates).
      clientRuntimeDeps = pkgs: with pkgs; [
        libGL
        libxkbcommon
        wayland
        mesa
        vulkan-loader
        libx11
        libxcursor
        libxrandr
        libxi
        alsa-lib
        udev
      ];

      mkRustPackage = pkgs:
        { pname # also the binary name
        , package # cargo package (-p) to build
        , runtimeDeps ? [ ]
        , linkDeps ? [ ] # link-time libraries (alsa, udev, ...)
        }:
        pkgs.rustPlatform.buildRustPackage {
          inherit pname version;
          src = pkgs.lib.cleanSource self;
          cargoLock.lockFile = ./Cargo.lock;

          buildType = "release";
          cargoBuildFlags = [ "--package" package ];

          # Tests are run explicitly when they exist; don't pay for them on
          # every build of this large workspace.
          doCheck = false;

          nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
          buildInputs = linkDeps;

          postInstall = pkgs.lib.optionalString (runtimeDeps != [ ]) ''
            wrapProgram "$out/bin/${pname}" \
              --suffix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeDeps}"
          '';

          meta = {
            mainProgram = pname;
            license = pkgs.lib.licenses.agpl3Only;
          };
        };

    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgsFor system;
          ssc-client = mkRustPackage pkgs {
            pname = "ssc-client";
            package = "ssc-client";
            runtimeDeps = clientRuntimeDeps pkgs;
            # cpal links ALSA, gilrs links udev.
            linkDeps = with pkgs; [ alsa-lib udev ];
          };
          ssc-server = mkRustPackage pkgs {
            pname = "ssc-server";
            package = "ssc-server";
          };
        in
        {
          inherit ssc-client ssc-server;
          # `nix run .` / `nix build .` default to the client.
          default = ssc-client;
        });

      apps = forAllSystems (system:
        let
          clientBin = "${self.packages.${system}.ssc-client}/bin/ssc-client";
          serverBin = "${self.packages.${system}.ssc-server}/bin/ssc-server";
        in
        {
          ssc-client = { type = "app"; program = clientBin; };
          ssc-server = { type = "app"; program = serverBin; };
          default = { type = "app"; program = clientBin; };
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgsFor system; in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              pkg-config
            ];
            # Link-time libraries for cargo builds in the shell (ALSA/udev),
            # plus runtime dlopen paths for the client.
            NIX_LDFLAGS = "-L${pkgs.alsa-lib}/lib -L${pkgs.udev}/lib";
            PKG_CONFIG_PATH = "${pkgs.alsa-lib.dev or pkgs.alsa-lib}/lib/pkgconfig:${pkgs.udev.dev or pkgs.udev}/lib/pkgconfig";
            # rust-analyzer needs the rust sources of the exact toolchain.
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # Let the client run directly from the dev shell.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (clientRuntimeDeps pkgs);
          };
        });
    };
}
