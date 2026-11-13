{ self, pkgs }:
let
  # The fake engine logs every request next to its script, so the script is copied into the
  # state directory when the service starts.
  script = pkgs.writeText "fake-engine.json" (
    builtins.toJSON {
      transcribe = [
        {
          kind = "result";
          text = "hello from PSST on nix os";
        }
      ];
    }
  );
  fakeEngine = pkgs.writeShellScriptBin "psst-engine-fake" ''
    exec ${pkgs.psst-server}/bin/psst-server __fake-engine /var/lib/psst/fake-engine.json
  '';
  # In place of `psst daemon`, which needs audio.
  dictate =
    pkgs.writers.writePython3Bin "psst-dictate"
      {
        libraries = [ pkgs.python3Packages.websockets ];
        flakeIgnore = [ "E501" ];
      }
      ''
        import json
        import uuid

        from websockets.sync.client import unix_connect

        SAMPLES = 16000
        FRAME = 320


        def answer(ws):
            while True:
                msg = json.loads(ws.recv(timeout=30))
                if msg["type"] != "progress":
                    print(json.dumps(msg), flush=True)
                    return msg


        with unix_connect("/run/psst/client.sock", "ws://localhost/v1/client") as ws:
            ws.send(json.dumps({"type": "hello", "protocol": 1, "client": {"name": "vm-test", "version": "0"}}))
            if answer(ws)["type"] == "welcome":
                session = str(uuid.uuid4())
                audio = {"codec": "pcm_s16le", "sample_rate": 16000, "frame_ms": 20}
                ws.send(json.dumps({"type": "start", "session_id": session, "audio": audio, "mode": "upload"}))
                ready = json.loads(ws.recv(timeout=30))
                if ready["type"] != "ready":
                    print(json.dumps(ready), flush=True)
                else:
                    for _ in range(SAMPLES // FRAME):
                        ws.send(bytes(2 * FRAME))
                    ws.send(json.dumps({"type": "stop", "session_id": session, "samples": SAMPLES}))
                    answer(ws)
      '';
  # tailscaled's LocalAPI, saying that 127.0.0.1 is the tailnet machine `thinkpad`.
  fakeTailscaled =
    pkgs.writers.writePython3 "fake-tailscaled"
      {
        flakeIgnore = [ "E501" ];
      }
      ''
        import json
        import os
        import socketserver
        from http.server import BaseHTTPRequestHandler

        SOCKET = "/run/tailscale/tailscaled.sock"
        WHOIS = {"Node": {"StableID": "nThinkpadCNTRL", "Name": "thinkpad.tail1234.ts.net."}}
        STATUS = {"CurrentTailnet": {"MagicDNSSuffix": "tail1234.ts.net"}}


        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.startswith("/localapi/v0/whois?addr=127.0.0.1"):
                    self.answer(200, WHOIS)
                elif self.path == "/localapi/v0/status?peers=false":
                    self.answer(200, STATUS)
                else:
                    self.answer(404, "no match for IP:port")

            def answer(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def address_string(self):
                return "local"


        os.makedirs("/run/tailscale", exist_ok=True)
        if os.path.exists(SOCKET):
            os.remove(SOCKET)
        with socketserver.UnixStreamServer(SOCKET, Handler) as server:
            os.chmod(SOCKET, 0o666)
            server.serve_forever()
      '';
in
pkgs.testers.runNixOSTest {
  name = "psst-server";

  nodes.machine = {
    imports = [ self.nixosModules.psst-server ];

    users.users.alice.isNormalUser = true;
    users.users.bob.isNormalUser = true;
    environment.systemPackages = [
      pkgs.curl
      pkgs.socat
      dictate
    ];
    systemd.services.fake-tailscaled = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.ExecStart = fakeTailscaled;
    };

    services.psst-server = {
      enable = true;
      enginePackage = fakeEngine;
      settings.server = {
        local_users = [ "alice" ];
        auto_pair = [ "thinkpad" ];
        model = {
          command = [ "psst-engine-fake" ];
          device = "cpu";
          compute_type = "int8";
        };
      };
      settings.profiles.profiles.default.prompt_style = "Testing psst in a VM.";
      settings.dictionary.terms = [
        {
          text = "NixOS";
          sounds_like = [ "nix os" ];
        }
      ];
    };
    systemd.services.psst-server.preStart = "install -m 0644 ${script} /var/lib/psst/fake-engine.json";
  };

  testScript = ''
    import json

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
        machine.succeed("grep -Fx 'text = \"NixOS\"' /etc/psst/dictionary.toml")
        machine.succeed("psst-server check-config /etc/psst")

    with subtest("TCP is opened on tailscale0 only"):
        rules = [l for l in machine.succeed("iptables-save; ip6tables-save").splitlines() if "7341" in l]
        assert len(rules) == 2 and all("-i tailscale0" in l for l in rules), rules

    with subtest("every local user reaches the socket; the server decides whom it serves"):
        for user in ["alice", "bob"]:
            machine.succeed(f"su {user} -c 'socat -u /dev/null UNIX-CONNECT:/run/psst/client.sock'")

    def as_user(user, command):
        return f"su {user} -c '{command}'"

    def dictate(user):
        return [json.loads(line) for line in machine.succeed(as_user(user, "psst-dictate")).splitlines()]

    def engine_requests():
        log = machine.succeed("cat /var/lib/psst/fake-engine.requests.jsonl")
        return [json.loads(line)["header"] for line in log.splitlines()]

    def model_state():
        status, out = machine.execute(as_user("alice", "psst-server model status --json"))
        return json.loads(out)["state"] if status == 0 else None

    def loads():
        return [r for r in engine_requests() if r["type"] == "load"]

    with subtest("the server starts and preloads the model with the engine on its PATH"):
        machine.wait_for_unit("psst-server.service")
        retry(lambda _: model_state() == "loaded")
        assert [(r["model"], r["device"], r["compute_type"]) for r in loads()] == [("large-v3-turbo", "cpu", "int8")]

    with subtest("local_users are admins on the socket; every other user, root included, is refused"):
        machine.succeed(as_user("alice", "psst-server devices list"))
        for user in ["bob", "root"]:
            out = machine.fail(as_user(user, "psst-server model status 2>&1"))
            assert "local_users" in out and "403" in out, out
            [refusal] = dictate(user)
            assert (refusal["type"], refusal["code"]) == ("error", "unauthorized"), refusal

    with subtest("a dictation over the socket uses /etc/psst's profile and dictionary, over the built-in terms"):
        welcome, final = dictate("alice")
        assert welcome["type"] == "welcome" and welcome["device"] == "local:alice", welcome
        assert final["type"] == "final", final
        assert final["final"]["raw_text"] == "hello from PSST on nix os", final
        assert final["final"]["text"] == "Hello from psst on NixOS ", final
        [transcribe] = [r for r in engine_requests() if r["type"] == "transcribe"]
        assert transcribe["bias"] == {"style": "Testing psst in a VM.", "terms": ["psst", "NixOS"]}, transcribe
        [row] = json.loads(machine.succeed(as_user("alice", "psst-server history --json")))
        assert (row["id"], row["device"]) == (final["history_id"], "local:alice"), row

    with subtest("a machine in auto_pair pairs itself: the sandboxed service reaches tailscaled's socket"):
        machine.wait_until_succeeds("test -S /run/tailscale/tailscaled.sock")
        paired = json.loads(machine.succeed("curl -sf -X POST http://127.0.0.1:7341/api/pair/auto"))
        assert paired["token"].startswith("psst_"), paired
        devices = json.loads(machine.succeed(as_user("alice", "psst-server devices list --json")))
        assert any(d["id"] == paired["device_id"] and d["name"] == "thinkpad" for d in devices), devices

    with subtest("reload rereads profiles.toml and dictionary.toml, keeping the process and the engine"):
        pid = show("psst-server.service", "MainPID")
        machine.succeed("systemctl reload psst-server.service")
        for file in ["profiles.toml", "dictionary.toml"]:
            machine.wait_until_succeeds(f"journalctl --all -u psst-server.service | grep -F '{file}: reloaded'")
        assert show("psst-server.service", "MainPID") == pid
        assert model_state() == "loaded"
        assert len(loads()) == 1

    with subtest("a model unloaded on purpose stays unloaded across a restart until `model load`"):
        machine.succeed(as_user("alice", "psst-server model unload"))
        assert model_state() == "unloaded"
        [_, refusal] = dictate("alice")
        assert (refusal["type"], refusal["code"]) == ("error", "model_unloaded"), refusal
        machine.succeed("systemctl restart psst-server.service")
        machine.wait_for_unit("psst-server.service")
        retry(lambda _: model_state() is not None)
        assert model_state() == "unloaded"
        machine.succeed(as_user("alice", "psst-server model load"))
        assert model_state() == "loaded"
        assert len(loads()) == 2
  '';
}
