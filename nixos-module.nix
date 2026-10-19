# NixOS module for the clinic appointment notifier (invitro-bot).
#
# Usage in configuration.nix (default: one unit watching Invitro.ru):
#
#   services.invitroBot = {
#     enable = true;
#     package = inputs.invitro-notification-bot.packages.${pkgs.system}.default;
#     environmentFile = "/run/secrets/invitro-bot.env"; # INVITRO_TELEGRAM_BOT_TOKEN, INVITRO_TELEGRAM_CHAT_ID
#   };
#
# To watch a second clinic at the same time (e.g. МЦ «Харизма»), add an
# instance — each one becomes its own systemd unit and state file:
#
#   services.invitroBot = {
#     enable = true;
#     package = ...;
#     environmentFile = "/run/secrets/invitro-bot.env";
#     instances.harizma = { provider = "harizma"; };   # unit invitro-bot-harizma
#   };
#
{ config, lib, pkgs, ... }:

let
  cfg = config.services.invitroBot;

  # Options shared by every watch instance: the legacy top-level options of the
  # "default" instance and each entry of `services.invitroBot.instances.<name>`.
  instanceOptions = {
    provider = lib.mkOption {
      type = lib.types.str;
      default = "invitro";
      description = ''
        Clinic booking system to watch: `invitro` (default) or `harizma`
        (harizma45.ru, МЦ «Харизма», Курган).
      '';
    };

    doctorBitrixId = lib.mkOption {
      type = lib.types.int;
      default = 19143;
      description = "Numeric doctor id from the Invitro.ru page URL.";
    };

    doctorName = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Alternative to doctorBitrixId: find the doctor by name. Exact full-name match is preferred; ambiguous names are rejected.";
    };

    citySlug = lib.mkOption {
      type = lib.types.str;
      default = "kurgan";
      description = "City slug from the page URL.";
    };

    specialtySlug = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Specialty slug to watch; defaults to the doctor's primary specialty.";
    };

    serviceIds = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Service UUIDs to watch. If empty, the doctor's main service at each
        office (the one used by the site's booking flow) is watched.
      '';
    };

    harizmaDoctorId = lib.mkOption {
      type = lib.types.int;
      default = 940;
      description = "Internal schedule id of the harizma45.ru doctor (default: Вождаева, id 940).";
    };

    harizmaPagePath = lib.mkOption {
      type = lib.types.str;
      default = "/register/uzi-diagnostika/vozhdaeva-anastasiya/";
      description = "Booking page path on harizma45.ru for the watched doctor.";
    };

    harizmaDoctorName = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Alternative to harizmaDoctorId: find the doctor by name on the harizma45.ru register page. Exact full-name match is preferred; ambiguous names are rejected.";
    };

    pollIntervalSecs = lib.mkOption {
      type = lib.types.int;
      default = 120;
      description = "Seconds between polls of the clinic site.";
    };

    notifyOnFirstRun = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Notify about slots that are already open on the very first poll.";
    };

    stateFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Path of the de-duplication state file. Defaults to /var/lib/<stateDirectory>/state.json.";
    };

    stateDirectory = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "systemd StateDirectory name. Defaults to `invitro-bot` for the default instance and `invitro-bot-<name>` for named instances.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Additional INVITRO_*/HARIZMA_* environment variables for this instance (merged with the top-level ones).";
    };
  };

  # Resolve the effective runtime values of one instance.
  effective = { name, inst }:
    let
      stateDirectory = if inst.stateDirectory != null then inst.stateDirectory
        else if name == "default" then "invitro-bot"
        else "invitro-bot-${name}";
      stateFile = if inst.stateFile != null then inst.stateFile
        else "/var/lib/${stateDirectory}/state.json";
      env = [
        "INVITRO_PROVIDER=${inst.provider}"
        "INVITRO_DOCTOR_BITRIX_ID=${toString inst.doctorBitrixId}"
        "INVITRO_CITY_SLUG=${inst.citySlug}"
        "INVITRO_POLL_INTERVAL_SECS=${toString inst.pollIntervalSecs}"
        "INVITRO_STATE_FILE=${stateFile}"
        "INVITRO_NOTIFY_ON_FIRST_RUN=${if inst.notifyOnFirstRun then "1" else "0"}"
        "HARIZMA_DOCTOR_ID=${toString inst.harizmaDoctorId}"
        "HARIZMA_PAGE_PATH=${inst.harizmaPagePath}"
      ] ++ lib.optional (inst.doctorName != null) "INVITRO_DOCTOR_NAME=${inst.doctorName}"
        ++ lib.optional (inst.specialtySlug != null) "INVITRO_SPECIALTY_SLUG=${inst.specialtySlug}"
        ++ lib.optional (inst.serviceIds != []) "INVITRO_SERVICE_IDS=${lib.concatStringsSep "," inst.serviceIds}"
        ++ lib.optional (inst.harizmaDoctorName != null) "HARIZMA_DOCTOR_NAME=${inst.harizmaDoctorName}"
        ++ lib.mapAttrsToList (n: v: "${n}=${v}") (cfg.extraEnvironment // inst.extraEnvironment);
    in
    { inherit stateDirectory stateFile env; };

  # Build one hardened systemd unit for an instance.
  mkUnit = { name, inst, environmentFile, description }:
    let
      eff = effective { inherit name inst; };
    in
    {
      description = description;
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/invitro-bot";
        Restart = "always";
        RestartSec = "10s";

        Environment = eff.env;
        EnvironmentFile = lib.optional (environmentFile != null) environmentFile;

        StateDirectory = eff.stateDirectory;
        DynamicUser = true;

        # Hardening: the bot only needs outbound HTTPS.
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
      };
    };
in
{
  options.services.invitroBot = {
    enable = lib.mkEnableOption "clinic doctor appointment Telegram notifier";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The invitro-bot package to run.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Path to an EnvironmentFile with secrets, at least:
          INVITRO_TELEGRAM_BOT_TOKEN=<token from @BotFather>
          INVITRO_TELEGRAM_CHAT_ID=<chat id to notify>
        Instances inherit this unless they set their own `environmentFile`.
      '';
    };

    enableDefaultInstance = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Create the default `invitro-bot` unit from the top-level options (the legacy single-instance configuration).";
    };

    instances = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options = instanceOptions // {
          enable = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Create the systemd unit for this instance (disabled instances stay configured but do not run).";
          };
          environmentFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = "Override the top-level environmentFile for this instance.";
          };
        };
      });
      default = { };
      description = ''
        Additional watch instances. Each becomes its own hardened systemd
        unit `invitro-bot-<name>` with its own state directory, so several
        clinics can be watched at the same time, e.g.:

        instances.harizma.provider = "harizma"; # Вождаева on harizma45.ru
      '';
    };
  } // instanceOptions;

  config = lib.mkIf cfg.enable {
    systemd.services =
      (if cfg.enableDefaultInstance
        then {
          "invitro-bot" = mkUnit {
            name = "default";
            inst = cfg;
            environmentFile = cfg.environmentFile;
            description = "Clinic doctor slot notifier";
          };
        }
        else { })
      // lib.mapAttrs' (name: inst:
        lib.nameValuePair "invitro-bot-${name}" (mkUnit {
          inherit name;
          inst = inst;
          environmentFile = if inst.environmentFile != null then inst.environmentFile else cfg.environmentFile;
          description = "Clinic doctor slot notifier (${name})";
        })
      ) (lib.filterAttrs (_: inst: inst.enable) cfg.instances);
  };
}
