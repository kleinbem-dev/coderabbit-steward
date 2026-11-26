{
  lib,
  rustPlatform,
  buildNpmPackage,
  pkg-config,
  sqlite,
  versionCheckHook,
  nix-update-script,
  runCommand,
  curl,
}:

let
  version = "0.1.0";

  localSrc = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../README.md
      ../LICENSE
      ../migrations
      ../src
      ../tests
      ../web/package.json
      ../web/package-lock.json
      ../web/.npmrc
      ../web/tsconfig.json
      ../web/vite.config.ts
      ../web/src
      ../web/static
    ];
  };

  web = buildNpmPackage {
    pname = "coderabbit-steward-web";
    inherit version;
    src = "${localSrc}/web";

    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-Smyeg7CQQcdVCS8DKlhRFTrntrO9MiDoNMmCyOnwW3A=";

    strictDeps = true;
    __structuredAttrs = true;

    doCheck = true;
    checkPhase = ''
      runHook preCheck
      npm run check
      runHook postCheck
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -r build/. $out/
      runHook postInstall
    '';
  };
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "coderabbit-steward";
  inherit version;
  src = localSrc;

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  strictDeps = true;
  __structuredAttrs = true;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];

  env = {
    LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
  };

  checkType = "debug";

  postInstall = ''
    mkdir -p $out/share/coderabbit-steward/web
    cp -r ${finalAttrs.passthru.web}/. $out/share/coderabbit-steward/web/
  '';

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  passthru = {
    inherit web;
    updateScript = nix-update-script { };
    tests = {
      daemon-smoke =
        runCommand "coderabbit-steward-daemon-smoke"
          {
            nativeBuildInputs = [
              finalAttrs.finalPackage
              curl
            ];
          }
          ''
            export HOME="$TMPDIR/home"
            mkdir -p "$HOME"

            cr-steward daemon \
              --listen 127.0.0.1:18484 \
              --database-url "sqlite://$TMPDIR/steward.db?mode=rwc" &
            DAEMON_PID=$!
            trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

            ready=0
            for _ in $(seq 1 30); do
              if curl -fsS http://127.0.0.1:18484/health >/dev/null 2>&1; then
                ready=1
                break
              fi
              sleep 0.2
            done
            test "$ready" -eq 1

            health="$(curl -fsS http://127.0.0.1:18484/health)"
            grep -q '"status":"healthy"' <<<"$health"

            budget_json="$(curl -fsS http://127.0.0.1:18484/api/v1/budget)"
            grep -q '^\[' <<<"$budget_json"

            index_html="$(curl -fsS http://127.0.0.1:18484/)"
            grep -qi '<!doctype html>' <<<"$index_html"

            kill "$DAEMON_PID"
            wait "$DAEMON_PID" || true
            touch "$out"
          '';
    };
  };

  meta = {
    description = "Budget tracker, scheduler, coverage map, and findings steward for CodeRabbit OSS";
    homepage = "https://github.com/kleinbem-dev/coderabbit-steward";
    license = lib.licenses.mit;
    maintainers = with lib.maintainers; [ kleinbem ];
    mainProgram = "cr-steward";
    platforms = lib.platforms.unix;
  };
})
