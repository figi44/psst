overlay:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;

  cfg = config.services.psst-server;
  psstPkgs = overlay pkgs pkgs;
  toml = pkgs.formats.toml { };

  files = {
    "server.toml" = toml.generate "server.toml" cfg.settings.server;
    "profiles.toml" = toml.generate "profiles.toml" cfg.settings.profiles;
    "dictionary.toml" = toml.generate "dictionary.toml" cfg.settings.dictionary;
  };

  configDir =
    pkgs.runCommand "psst-server-config"
      {
        nativeBuildInputs = [ cfg.package ];
      }
      ''
        mkdir $out
        ${lib.concatLines (lib.mapAttrsToList (name: file: "cp ${file} $out/${name}") files)}
        psst-server check-config $out
      '';

  # Both "0.0.0.0:7341" and "[::]:7341" end in their port.
  portOf =
    address:
    let
      port = builtins.match ".*:([0-9]+)" address;
    in
    if port == null then
      throw "services.psst-server.settings.server.listen: `${address}` has no port"
    else
      lib.toInt (builtins.head port);
in
{
  options.services.psst-server = {
    enable = lib.mkEnableOption "the psst dictation server";

    package = mkOption {
      type = types.package;
      default = psstPkgs.psst-server;
      defaultText = lib.literalMD "`psst-server` from psst's overlay, built from this host's `pkgs`";
      description = ''
        The psst-server package. It's also installed system-wide for the admin commands, such as
        `psst-server model unload`.
      '';
    };

    enginePackage = mkOption {
      type = types.nullOr types.package;
      default = psstPkgs.psst-engine-faster-whisper-cuda;
      defaultText = lib.literalMD "`psst-engine-faster-whisper-cuda` from psst's overlay, built from this host's `pkgs`";
      description = ''
        The engine, put on the service's `PATH` so that `settings.server.model.command` can name
        it. The default needs `nixpkgs.config.allowUnfree`. `null` adds nothing.
      '';
    };

    settings = {
      server = mkOption {
        type = types.submodule {
          freeformType = toml.type;
          options = {
            listen = mkOption {
              type = types.listOf types.str;
              default = [
                "0.0.0.0:7341"
                "[::]:7341"
              ];
              description = ''
                TCP listeners for other devices. Their ports are opened in the firewall on
                `firewall.interfaces` only. `[ ]` leaves only the socket.
              '';
            };
            socket = mkOption {
              type = types.strMatching "/.+";
              default = "/run/psst/client.sock";
              description = ''
                The Unix socket for users on this machine. `psst-server.socket` listens on it with
                mode 0666 and hands it to the server, which accepts only `local_users`.
              '';
            };
            local_users = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [ "alice" ];
              description = ''
                Users on this machine the server accepts on the socket, as admins. They need no
                pairing.
              '';
            };
            auto_pair = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [ "thinkpad" ];
              description = ''
                Tailnet machines, by MagicDNS short name, whose daemons pair themselves on their
                first connection: the server asks tailscaled (`tailscale_socket`) which machine is
                calling. Each gets one device; one removed from the list is revoked when the server
                restarts.
              '';
            };
          };
        };
        default = { };
        description = ''
          `server.toml`; `psst-config` documents every key and its default. A change restarts the
          server.
        '';
      };

      profiles = mkOption {
        inherit (toml) type;
        default = { };
        example = lib.literalExpression ''
          {
            profiles.default = {
              language = "en";
              prompt_style = "I'm updating the NixOS config.";
            };
            profiles.czech.language = "cs";
          }
        '';
        description = ''
          `profiles.toml`. A change reloads the server (`SIGHUP`) without restarting the engine.
        '';
      };

      dictionary = mkOption {
        inherit (toml) type;
        default = { };
        example = lib.literalExpression ''
          {
            terms = [
              { text = "Hyprland"; sounds_like = [ "hyper land" ]; }
              { text = "psst"; pin = true; }
            ];
          }
        '';
        description = ''
          `dictionary.toml`. A change reloads the server (`SIGHUP`) without restarting the engine.
        '';
      };
    };

    firewall.interfaces = mkOption {
      type = types.listOf types.str;
      default = [ "tailscale0" ];
      description = ''
        Interfaces on which the `listen` ports are opened. psst has no TLS, so open them only on
        an encrypted network such as Tailscale's. The server itself also refuses connections from
        beyond `settings.server.allow_from`, which by default is this machine and its tailnet.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    warnings = lib.optional (cfg.settings.server.local_users == [ ]) ''
      services.psst-server.settings.server.local_users is empty, so no user on this machine can
      use psst-server's socket.
    '';

    environment.systemPackages = [ cfg.package ];
    environment.etc.psst.source = configDir;

    networking.firewall.interfaces = lib.genAttrs cfg.firewall.interfaces (_: {
      allowedTCPPorts = lib.unique (map portOf cfg.settings.server.listen);
    });

    # systemd creates the socket's directory and owns the socket, so the service has no
    # RuntimeDirectory, which would delete both whenever the service stops.
    systemd.sockets.psst-server = {
      description = "psst dictation server socket";
      wantedBy = [ "sockets.target" ];
      listenStreams = [ cfg.settings.server.socket ];
      socketConfig = {
        SocketMode = "0666";
        DirectoryMode = "0755";
      };
    };

    systemd.services.psst-server = {
      description = "psst dictation server";
      # Started at boot rather than on first use, so the model preloads.
      wantedBy = [ "multi-user.target" ];
      requires = [ "psst-server.socket" ];
      after = [
        "psst-server.socket"
        "network.target"
      ];
      path = [ cfg.package ] ++ lib.optional (cfg.enginePackage != null) cfg.enginePackage;
      environment.HF_HOME = "/var/cache/psst";

      restartTriggers = [ files."server.toml" ];
      reloadTriggers = [
        files."profiles.toml"
        files."dictionary.toml"
      ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} serve --config-dir /etc/psst --state-dir /var/lib/psst --cache-dir /var/cache/psst";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        RestartSec = 5;

        # Also implies NoNewPrivileges, ProtectSystem=strict and a private /tmp.
        DynamicUser = true;
        StateDirectory = "psst";
        CacheDirectory = "psst";
        UMask = "0077";

        # NVIDIA's device nodes are mode 0666 on NixOS; other GPU drivers' belong to these groups.
        # PrivateDevices, DeviceAllow and ProtectClock (which implies a DeviceAllow) stay unset, since
        # any of them would hide /dev/nvidia*. MemoryDenyWriteExecute stays unset for CUDA's JIT.
        SupplementaryGroups = [
          "video"
          "render"
        ];

        CapabilityBoundingSet = "";
        LockPersonality = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
          "AF_NETLINK"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
