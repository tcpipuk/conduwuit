# Dependencies (keep sorted)
{ craneLib
, inputs
, jq
, lib
, libiconv
, liburing
, pkgsBuildHost
, rocksdb
, rust
, rust-jemalloc-sys
, stdenv

# Options (keep sorted)
, default_features ? true
, disable_release_max_log_level ? false
, all_features ? false
, disable_features ? []
, features ? []
, profile ? "release-high-perf"
}:

let
# We perform default-feature unification in nix, because some of the dependencies
# on the nix side depend on feature values.
workspaceMembers = builtins.map (member: "${inputs.self}/src/${member}")
  (builtins.attrNames (builtins.readDir "${inputs.self}/src"));
crateFeatures = path:
  let manifest = lib.importTOML "${path}/Cargo.toml"; in
  lib.remove "default" (lib.attrNames manifest.features) ++
  lib.attrNames
    (lib.filterAttrs
      (_: dependency: dependency.optional or false)
      manifest.dependencies);
crateDefaultFeatures = path:
  (lib.importTOML "${path}/Cargo.toml").features.default;
allDefaultFeatures = lib.unique
  (lib.flatten (builtins.map crateDefaultFeatures workspaceMembers));
allFeatures = lib.unique
  (lib.flatten (builtins.map crateFeatures workspaceMembers));
features' = lib.unique
  (features ++
    lib.optionals default_features allDefaultFeatures ++
    lib.optionals all_features allFeatures);
disable_features' = disable_features ++ lib.optionals disable_release_max_log_level ["release_max_log_level"];
features'' = lib.subtractLists disable_features' features';

featureEnabled = feature : builtins.elem feature features'';

enableLiburing = featureEnabled "io_uring" && stdenv.isLinux;

# This derivation will set the JEMALLOC_OVERRIDE variable, causing the
# tikv-jemalloc-sys crate to use the nixpkgs jemalloc instead of building it's
# own. In order for this to work, we need to set flags on the build that match
# whatever flags tikv-jemalloc-sys was going to use. These are dependent on
# which features we enable in tikv-jemalloc-sys.
rust-jemalloc-sys' = (rust-jemalloc-sys.override {
  # tikv-jemalloc-sys/unprefixed_malloc_on_supported_platforms feature
  unprefixed = true;
}).overrideAttrs (old: {
  configureFlags = old.configureFlags ++
    # tikv-jemalloc-sys/profiling feature
    lib.optional (featureEnabled "jemalloc_prof") "--enable-prof";
});

buildDepsOnlyEnv =
  let
    rocksdb' = (rocksdb.override {
      jemalloc = rust-jemalloc-sys';
      # rocksdb fails to build with prefixed jemalloc, which is required on
      # darwin due to [1]. In this case, fall back to building rocksdb with
      # libc malloc. This should not cause conflicts, because all of the
      # jemalloc symbols are prefixed.
      #
      # [1]: https://github.com/tikv/jemallocator/blob/ab0676d77e81268cd09b059260c75b38dbef2d51/jemalloc-sys/src/env.rs#L17
      enableJemalloc = featureEnabled "jemalloc" && !stdenv.isDarwin;
    }).overrideAttrs (old: {
      # TODO: static rocksdb fails to build on darwin
      # build log at <https://girlboss.ceo/~strawberry/pb/JjGH>
      meta.broken = stdenv.hostPlatform.isStatic && stdenv.isDarwin;
      # TODO: switch to enableUring option once https://github.com/NixOS/nixpkgs/pull/314945 is available
      buildInputs = old.buildInputs ++ lib.optional enableLiburing liburing;
    });
  in
  {
    # https://crane.dev/faq/rebuilds-bindgen.html
    NIX_OUTPATH_USED_AS_RANDOM_SEED = "aaaaaaaaaa";

    CARGO_PROFILE = profile;
    ROCKSDB_INCLUDE_DIR = "${rocksdb'}/include";
    ROCKSDB_LIB_DIR = "${rocksdb'}/lib";
  }
  //
  (import ./cross-compilation-env.nix {
    # Keep sorted
    inherit
      lib
      pkgsBuildHost
      rust
      stdenv;
  });

buildPackageEnv = {
  CONDUWUIT_VERSION_EXTRA = inputs.self.shortRev or inputs.self.dirtyShortRev or "";
} // buildDepsOnlyEnv // {
  # Only needed in static stdenv because these are transitive dependencies of rocksdb
  CARGO_BUILD_RUSTFLAGS = buildDepsOnlyEnv.CARGO_BUILD_RUSTFLAGS
    + lib.optionalString (enableLiburing && stdenv.hostPlatform.isStatic)
      " -L${lib.getLib liburing}/lib -luring";
};



commonAttrs = {
  inherit
    (craneLib.crateNameFromCargoToml {
      cargoToml = "${inputs.self}/Cargo.toml";
    })
    pname
    version;

    src = let filter = inputs.nix-filter.lib; in filter {
      root = inputs.self;

      # Keep sorted
      include = [
        "Cargo.lock"
        "Cargo.toml"
        "deps"
        "src"
      ];
    };

    buildInputs = lib.optional (featureEnabled "jemalloc") rust-jemalloc-sys';

    nativeBuildInputs = [
      # bindgen needs the build platform's libclang. Apparently due to "splicing
      # weirdness", pkgs.rustPlatform.bindgenHook on its own doesn't quite do the
      # right thing here.
      pkgsBuildHost.rustPlatform.bindgenHook

      # We don't actually depend on `jq`, but crane's `buildPackage` does, but
      # its `buildDepsOnly` doesn't. This causes those two derivations to have
      # differing values for `NIX_CFLAGS_COMPILE`, which contributes to spurious
      # rebuilds of bindgen and its depedents.
      jq
  ]
  ++ lib.optionals stdenv.isDarwin [
      # https://github.com/NixOS/nixpkgs/issues/206242
      libiconv

      # https://stackoverflow.com/questions/69869574/properly-adding-darwin-apple-sdk-to-a-nix-shell
      # https://discourse.nixos.org/t/compile-a-rust-binary-on-macos-dbcrossbar/8612
      pkgsBuildHost.darwin.apple_sdk.frameworks.Security
    ];
 };
in

craneLib.buildPackage ( commonAttrs // {
  cargoArtifacts = craneLib.buildDepsOnly (commonAttrs // {
    env = buildDepsOnlyEnv;
  });

  cargoExtraArgs = "--no-default-features "
    + lib.optionalString
      (features'' != [])
      "--features " + (builtins.concatStringsSep "," features'');

  # This is redundant with CI
  cargoTestCommand = "";
  cargoCheckCommand = "";
  doCheck = false;

  env = buildPackageEnv;

  passthru = {
    env = buildPackageEnv;
  };

  meta.mainProgram = commonAttrs.pname;
})
