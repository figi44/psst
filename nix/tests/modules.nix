# Only the config files are built: the CUDA engine is evaluated, never built.
{
  self,
  nixpkgs,
  home-manager,
  pkgs,
}:
let
  inherit (pkgs) lib;

  nixos =
    module:
    nixpkgs.lib.nixosSystem {
      modules = [
        self.nixosModules.psst-server
        module
        {
          nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          system.stateVersion = "26.05";
        }
      ];
    };

  desktop = nixos {
    nixpkgs.config.allowUnfree = true;
    services.psst-server = {
      enable = true;
      settings.server.local_users = [ "alice" ];
    };
  };
  noUsers = nixos {
    services.psst-server = {
      enable = true;
      enginePackage = null;
    };
  };
  # profiles.toml without a `default` profile.
  invalid = nixos {
    services.psst-server = {
      enable = true;
      enginePackage = null;
      settings.server.local_users = [ "alice" ];
      settings.profiles.profiles.czech.language = "cs";
    };
  };

  home = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.psst
      {
        home = {
          username = "alice";
          homeDirectory = "/home/alice";
          stateVersion = "26.05";
        };
        services.psst = {
          enable = true;
          settings.server = "http://desktop:7341";
        };
      }
    ];
  };

  cfg = desktop.config;
  service = cfg.systemd.services.psst-server;
  socket = cfg.systemd.sockets.psst-server;
  daemon = home.config.systemd.user.services.psst;
  engine = lib.findFirst (p: lib.getName p == "psst-engine-faster-whisper") null service.path;

  expectations = {
    "the service starts at boot" = service.wantedBy == [ "multi-user.target" ];
    "the service needs its socket" = lib.elem "psst-server.socket" service.requires;
    "the service has a dynamic user" = service.serviceConfig.DynamicUser;
    "the service has state and cache directories" =
      service.serviceConfig.StateDirectory == "psst" && service.serviceConfig.CacheDirectory == "psst";
    "the service has no RuntimeDirectory" = !(service.serviceConfig ? RuntimeDirectory);
    "HF_HOME is the cache directory" = service.environment.HF_HOME == "/var/cache/psst";
    "the CUDA engine is on the service's PATH" = engine != null && engine.cudaSupport;
    "server.toml restarts, the others reload" =
      lib.length service.restartTriggers == 1 && lib.length service.reloadTriggers == 2;
    "the socket is /run/psst/client.sock" = socket.listenStreams == [ "/run/psst/client.sock" ];
    "the socket is open to every user" = socket.socketConfig.SocketMode == "0666";
    "TCP opens on tailscale0 only" =
      cfg.networking.firewall.interfaces.tailscale0.allowedTCPPorts == [ 7341 ]
      && cfg.networking.firewall.allowedTCPPorts == [ ];
    "admins get psst-server" = lib.any (
      p: lib.getName p == "psst-server"
    ) cfg.environment.systemPackages;
    "local users given, no warning" = !lib.any (lib.hasInfix "local_users") cfg.warnings;
    "no local users, a warning" = lib.any (lib.hasInfix "local_users") noUsers.config.warnings;
    "the daemon is part of the graphical session" =
      daemon.Unit.PartOf == [ "graphical-session.target" ]
      && daemon.Install.WantedBy == [ "graphical-session.target" ];
    "the daemon runs `psst daemon`" =
      daemon.Service.ExecStart == [ "${lib.getExe home.config.services.psst.package} daemon" ];
    "a client.toml change restarts the daemon" = daemon.Unit.X-Restart-Triggers != [ ];
    "psst is installed for the binds" = lib.any (p: lib.getName p == "psst") home.config.home.packages;
  };
  failed = lib.attrNames (lib.filterAttrs (_: ok: !ok) expectations);

  # Forces a full evaluation (assertions included) without building anything.
  evaluated = map (drv: builtins.unsafeDiscardStringContext drv.drvPath) [
    desktop.config.system.build.toplevel
    home.activationPackage
  ];

  rejected = pkgs.testers.testBuildFailure invalid.config.environment.etc.psst.source;
in
assert lib.assertMsg (failed == [ ]) "checks.modules: ${lib.concatStringsSep "; " failed}";
pkgs.runCommand "psst-modules"
  {
    inherit evaluated;
    serverConfig = cfg.environment.etc.psst.source;
    clientToml = home.config.xdg.configFile."psst/client.toml".source;
    inherit rejected;
  }
  ''
    grep -Fx 'local_users = ["alice"]' $serverConfig/server.toml
    grep -Fx 'socket = "/run/psst/client.sock"' $serverConfig/server.toml
    grep -Fx 'server = "http://desktop:7341"' $clientToml
    grep -F 'profiles.toml' $rejected/testBuildFailure.log
    touch $out
  ''
