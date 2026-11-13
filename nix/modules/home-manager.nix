overlay:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.psst;
  toml = pkgs.formats.toml { };
  clientToml = toml.generate "client.toml" cfg.settings;
in
{
  options.services.psst = {
    enable = lib.mkEnableOption "the psst dictation daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = (overlay pkgs pkgs).psst;
      defaultText = lib.literalMD "`psst` from psst's overlay, built from Home Manager's `pkgs`";
      description = "The psst package. It's also installed, for the compositor's binds.";
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          server = "http://desktop:7341";
          apps."org.keepassxc.KeePassXC".paste = false;
        }
      '';
      description = ''
        `client.toml`; `psst-config` documents every key and its default. A change restarts the
        daemon.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    xdg.configFile."psst/client.toml".source = clientToml;

    systemd.user.services.psst = {
      Unit = {
        Description = "psst dictation daemon";
        PartOf = [ "graphical-session.target" ];
        After = [
          "graphical-session.target"
          "pipewire.service"
        ];
        X-Restart-Triggers = [ "${clientToml}" ];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
