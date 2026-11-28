"""Hidden check: random message sequences replayed through the module by a driver, compared
step by step with a model of the specification."""
import json
import random
from checks import needs, same_on_engines, write


class Model:
    def __init__(self):
        self.todos, self.next = [], 1

    def update(self, msg):
        k = msg["kind"]
        if k == "add":
            text = msg["text"].strip()
            if text:
                self.todos.append({"id": self.next, "text": text, "done": False})
                self.next += 1
        elif k == "toggle":
            for t in self.todos:
                if t["id"] == msg["id"]:
                    t["done"] = not t["done"]
        elif k == "remove":
            self.todos = [t for t in self.todos if t["id"] != msg["id"]]
        elif k == "clear_done":
            self.todos = [t for t in self.todos if not t["done"]]

    def show(self, f):
        ts = [t for t in self.todos if f == "all" or (f == "done") == t["done"]]
        return ",".join("%d:%s:%s" % (t["id"], t["text"], "true" if t["done"] else "false") for t in ts)

    def summary(self):
        n = sum(1 for t in self.todos if not t["done"])
        return "1 item left" if n == 1 else "%d items left" % n


def helix_msg(m):
    parts = ['kind: "%s"' % m["kind"]]
    if "text" in m:
        parts.append("text: %s" % json.dumps(m["text"]))
    if "id" in m:
        parts.append("id: %d" % m["id"])
    return "{%s}" % ", ".join(parts)


def check(ws, helix):
    failures = needs(ws, "todo.helix")
    if failures:
        return failures
    rng = random.Random(68)
    words = ["milk", "  call mum ", "write ADR", "", "   ", "fix bug", "read"]
    lines = ["import todo.{init, update, visible, summary}", "",
             'fn show(s, f) = visible(s, f).map((t) => "{t.id}:{t.text}:{t.done}").join(",")', ""]
    want = []
    for run_no in range(4):
        m = Model()
        msgs = []
        for _ in range(40):
            k = rng.choice(["add", "add", "add", "toggle", "toggle", "remove", "clear_done"])
            if k == "add":
                msgs.append({"kind": k, "text": rng.choice(words)})
            elif k == "clear_done":
                msgs.append({"kind": k})
            else:
                msgs.append({"kind": k, "id": rng.randint(1, m.next + 1)})
        steps = []
        for msg in msgs:
            m.update(msg)
            steps.append((m.show("all"), m.show("active"), m.show("done"), m.summary()))
        lines.append("msgs%d = [%s]" % (run_no, ", ".join(helix_msg(x) for x in msgs)))
        lines.append("states%d = msgs%d.reduce([init()], (acc, msg) => acc.concat([update(acc[acc.count() - 1], msg)])).drop(1)" % (run_no, run_no))
        lines.append('printed%d = states%d.map((s) => print("{show(s, "all")}|{show(s, "active")}|{show(s, "done")}|{summary(s)}"))' % (run_no, run_no))
        want += ["|".join(s) for s in steps]
    write(ws, "zz_accept_driver.helix", "\n".join(lines) + "\n")
    return same_on_engines(helix, ws, ["run", "zz_accept_driver.helix"], want="\n".join(want))
