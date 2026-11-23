{
  config,
  lib,
  pkgs,
  options,
  ...
}:

let
  cfg = config.services.coderabbit-steward;
in
{
  options.services.coderabbit-steward = {
    enable = lib.mkEnableOption "coderabbit-steward daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.coderabbit-steward;
      defaultText = lib.literalExpression "pkgs.coderabbit-steward";
      description = "The coderabbit-steward package to run.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8484";
      description = "Address and port for the HTTP dashboard and API listener.";
    };

    mode = lib.mkOption {
      type = lib.types.enum [
        "observe"
        "on-demand"
        "full"
      ];
      default = "observe";
      description = "Operating mode for the steward.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "EnvironmentFile containing runtime secrets (GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY_FILE, CR_STEWARD_API_TOKEN).";
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      default = "sqlite:///var/lib/coderabbit-steward/steward.db?mode=rwc";
      description = "SQLite database URL.";
    };

    persistencePath = lib.mkOption {
      type = lib.types.str;
      default = "/nix/persist";
      description = "Root path for impermanence persistent storage when enabled on the host.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        systemd.services.coderabbit-steward = {
          description = "coderabbit-steward CodeRabbit OSS budget and review steward";
          after = [ "network.target" ];
          wantedBy = [ "multi-user.target" ];

          serviceConfig = {
            ExecStart = lib.escapeShellArgs [
              (lib.getExe cfg.package)
              "daemon"
              "--listen"
              cfg.listen
              "--mode"
              cfg.mode
              "--database-url"
              cfg.databaseUrl
            ];
            EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
            DynamicUser = true;
            StateDirectory = "coderabbit-steward";
            ProtectSystem = "strict";
            ProtectHome = true;
            NoNewPrivileges = true;
            PrivateTmp = true;
            Restart = "always";
            RestartSec = "5s";
          };
        };
      }
      (lib.optionalAttrs (options.environment ? persistence) {
        environment.persistence."${cfg.persistencePath}".directories = [
          "/var/lib/private/coderabbit-steward"
        ];
      })
    ]
  );
}
