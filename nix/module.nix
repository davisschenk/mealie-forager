self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.mealie-forager;
  inherit (lib) mkOption types;
in
{
  options.services.mealie-forager = {
    enable = lib.mkEnableOption "Mealie Forager, a social media and web recipe importer for Mealie";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "mealie-forager.packages.\${system}.default";
      description = "Mealie Forager package to run.";
    };

    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = "Address the web UI and API listen on.";
    };

    port = mkOption {
      type = types.port;
      default = 3000;
      description = "Port the web UI and API listen on.";
    };

    workers = mkOption {
      type = types.ints.positive;
      default = 2;
      description = "Number of jobs processed concurrently.";
    };

    mealieUrl = mkOption {
      type = types.str;
      example = "http://127.0.0.1:9000";
      description = "Mealie base URL used for API calls.";
    };

    mealiePublicUrl = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Mealie URL used for links in the UI; defaults to mealieUrl.";
    };

    mealieGroup = mkOption {
      type = types.str;
      default = "home";
      description = "Mealie group slug used when building recipe links.";
    };

    openaiUrl = mkOption {
      type = types.str;
      default = "https://api.openai.com/v1";
      description = "OpenAI-compatible API base URL.";
    };

    textModel = mkOption {
      type = types.str;
      default = "gpt-5-mini";
      description = "Chat model used to extract recipes.";
    };

    transcriptionModel = mkOption {
      type = types.str;
      default = "whisper-1";
      description = "Model used to transcribe audio.";
    };

    cleanup = mkOption {
      type = types.bool;
      default = true;
      description = "Clean imported recipes (link foods and units, tidy steps) and tag them.";
    };

    cleanTag = mkOption {
      type = types.str;
      default = "Imported Clean";
      description = "Tag added to recipes once they have been cleaned.";
    };

    environmentFile = mkOption {
      type = types.path;
      description = "File with OPENAI_API_KEY and MEALIE_API_KEY (and optionally COOKIES_FILE, EXTRA_PROMPT).";
    };

    settings = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = {
        MAX_DURATION_SECS = "900";
      };
      description = "Extra environment variables passed to the service.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.mealie-forager = {
      description = "Mealie Forager recipe importer";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        LISTEN_ADDR = "${cfg.listenAddress}:${toString cfg.port}";
        DATABASE_PATH = "/var/lib/mealie-forager/jobs.db";
        WORK_DIR = "/var/cache/mealie-forager";
        XDG_CACHE_HOME = "/var/cache/mealie-forager";
        WORKERS = toString cfg.workers;
        OPENAI_URL = cfg.openaiUrl;
        TEXT_MODEL = cfg.textModel;
        TRANSCRIPTION_MODEL = cfg.transcriptionModel;
        MEALIE_URL = cfg.mealieUrl;
        MEALIE_GROUP_NAME = cfg.mealieGroup;
        CLEANUP = lib.boolToString cfg.cleanup;
        CLEAN_TAG = cfg.cleanTag;
      }
      // lib.optionalAttrs (cfg.mealiePublicUrl != null) { MEALIE_PUBLIC_URL = cfg.mealiePublicUrl; }
      // cfg.settings;
      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        EnvironmentFile = cfg.environmentFile;
        DynamicUser = true;
        StateDirectory = "mealie-forager";
        CacheDirectory = "mealie-forager";
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
      };
    };
  };
}
