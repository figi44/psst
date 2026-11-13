# `checks.vm`: the NixOS module in a VM, with psst-server's scripted fake engine in place of the
# GPU engine.
{ self, pkgs }:
let
  # The fake engine logs every request next to its script, so the script is copied into the
  # state directory when the service starts.
  script = pkgs.writeText "fake-engine.json" (
    builtins.toJSON {
      transcribe = [
        {
          kind = "result";
          text = "hello from the VM";
        }
      ];
    }
  );
  # Stands in for the faster-whisper engine on the service's PATH.
  fakeEngine = pkgs.writeShellScriptBin "psst-engine-fake" ''
    exec ${pkgs.psst-server}/bin/psst-server __fake-engine /var/lib/psst/fake-engine.json
  '';
in
pkgs.testers.runNixOSTest {
  name = "psst-server";

  nodes.machine = {
    imports = [ self.nixosModules.psst-server ];

    users.users.alice.isNormalUser = true;
    users.users.bob.isNormalUser = true;
    environment.systemPackages = [ pkgs.socat ];

    services.psst-server = {
      enable = true;
      enginePackage = fakeEngine;
      settings.server = {
        local_users = [ "alice" ];
        model = {
          command = [ "psst-engine-fake" ];
          device = "cpu";
          compute_type = "int8";
        };
      };
      settings.profiles.profiles.default.prompt_style = "Testing psst in a VM.";
      settings.dictionary.terms = [ { text = "psst"; } ];
    };
    systemd.services.psst-server.preStart = "install -m 0644 ${script} /var/lib/psst/fake-engine.json";
  };

  testScript = ''
    machine.wait_for_unit("psst-server.socket")
    machine.wait_for_unit("multi-user.target")

    def show(unit, prop):
        return machine.succeed(f"systemctl show --property={prop} --value {unit}").strip()

    with subtest("systemd provides /run/psst/client.sock, mode 0666"):
        machine.succeed("test -S /run/psst/client.sock")
        assert machine.succeed("stat -c '%a %U' /run/psst/client.sock").strip() == "666 root"
        assert machine.succeed("stat -c '%a %U' /run/psst").strip() == "755 root"
        assert show("psst-server.socket", "Listen") == "/run/psst/client.sock (Stream)"

    with subtest("the service: started at boot, dynamic user, state and cache, no runtime directory"):
        assert "multi-user.target" in show("psst-server.service", "WantedBy").split()
        assert show("psst-server.service", "DynamicUser") == "yes"
        assert show("psst-server.service", "StateDirectory") == "psst"
        assert show("psst-server.service", "CacheDirectory") == "psst"
        assert show("psst-server.service", "RuntimeDirectory") == ""
        env = show("psst-server.service", "Environment")
        assert "HF_HOME=/var/cache/psst" in env.split(), env
        assert "${fakeEngine}/bin" in env, env
        assert "kill -HUP" in show("psst-server.service", "ExecReload")
        machine.succeed("cmp ${script} /var/lib/psst/fake-engine.json")

    with subtest("/etc/psst holds the settings, and they're valid"):
        machine.succeed("grep -Fx 'local_users = [\"alice\"]' /etc/psst/server.toml")
        machine.succeed("grep -Fx 'command = [\"psst-engine-fake\"]' /etc/psst/server.toml")
        machine.succeed("grep -Fx 'prompt_style = \"Testing psst in a VM.\"' /etc/psst/profiles.toml")
        machine.succeed("grep -Fx 'text = \"psst\"' /etc/psst/dictionary.toml")
        machine.succeed("psst-server check-config /etc/psst")

    with subtest("TCP is opened on tailscale0 only"):
        rules = [l for l in machine.succeed("iptables-save; ip6tables-save").splitlines() if "7341" in l]
        assert len(rules) == 2 and all("-i tailscale0" in l for l in rules), rules

    with subtest("every local user reaches the socket; the server decides whom it serves"):
        for user in ["alice", "bob"]:
            machine.succeed(f"su {user} -c 'socat -u /dev/null UNIX-CONNECT:/run/psst/client.sock'")

    # TODO(psst-server serve): `serve` is still a stub on main, so nothing answers on the
    # socket yet and the service keeps restarting. Once it serves, check the access rules
    # (plan 6.6) and the engine on PATH here:
    #   machine.wait_for_unit("psst-server.service")
    #   machine.succeed("su alice -c 'psst-server model status'")  # an allowed user, an admin
    #   out = machine.fail("su bob -c 'psst-server model status' 2>&1")
    #   assert "unauthorized" in out, out                          # anyone else is refused
    #   machine.wait_until_succeeds("grep -q '\"type\":\"load\"' /var/lib/psst/fake-engine.requests.jsonl")
  '';
}
