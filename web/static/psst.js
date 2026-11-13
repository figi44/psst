// The web UI's enhancements. Every page works without this file: it adds picking words in a
// dictation, live hints in the fix form, closing the menu and side panels, and the profile's
// token count. Handlers sit on `document`, so they keep working when htmx swaps the page.
(function () {
  "use strict";
  document.documentElement.classList.add("js");

  var $ = function (sel, root) { return (root || document).querySelector(sel); };
  var $$ = function (sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); };
  var esc = function (s) {
    return String(s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; });
  };
  var norm = function (s) { return String(s).trim().replace(/\s+/g, " ").toLowerCase(); };
  var tidy = function (s) { return String(s).replace(/\s+/g, " ").replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}+#]+$/gu, ""); };
  var fine = function () { return matchMedia("(pointer: fine)").matches; };

  // ---------- History: picking words ----------
  // A pick is a range of a dictation's children (words, gaps, dotted words): {text, a, b, anchor}.
  var pick = null;
  var kids = function (text) { return Array.prototype.slice.call(text.children); };
  var isWord = function (el) { return el && el.classList.contains("w"); };

  // The next word from index `i` in direction `dir`. With `phrase`, never past a sentence end or
  // a word just fixed.
  function neighbour(text, i, dir, phrase) {
    var all = kids(text);
    for (var j = i + dir; j >= 0 && j < all.length; j += dir) {
      if (isWord(all[j])) return j;
      if (phrase && (all[j].classList.contains("fixed") || /[.!?]/.test(all[j].textContent))) return -1;
    }
    return -1;
  }
  function heardOf(text, a, b) {
    var s = "";
    kids(text).slice(a, b + 1).forEach(function (el) { s += el.dataset.heard || el.firstChild.textContent; });
    return tidy(s);
  }
  function shownOf(text, a, b) {
    var s = "";
    kids(text).slice(a, b + 1).forEach(function (el) { s += el.firstChild.textContent; });
    return tidy(s);
  }
  // The dotted word a pick is, when it's exactly one.
  function dotted(text, a, b) {
    var el = kids(text)[a];
    return a === b && el && el.classList.contains("fx") ? el : null;
  }
  function paint() {
    $$(".text .in").forEach(function (el) { el.classList.remove("in", "first", "last"); });
    if (!pick) return;
    var all = kids(pick.text);
    for (var i = pick.a; i <= pick.b; i++) all[i].classList.add("in");
    all[pick.a].classList.add("first");
    all[pick.b].classList.add("last");
  }

  function keepText(fx) {
    var term = fx.dataset.term, heard = fx.dataset.heard;
    return fx.dataset.how === "fuzzy"
      ? "Keep as heard: " + esc(term) + " won't replace “" + esc(heard) + "” from your next dictation."
      : "Keep as heard: psst stops writing " + esc(term) + " for “" + esc(heard) + "” from your next dictation.";
  }
  function known(word) {
    var want = norm(word);
    return $$("#known-words option").map(function (o) { return o.value; }).filter(function (v) { return norm(v) === want; })[0];
  }
  // The message under "Write it as": what pressing the main button does.
  function hint(form) {
    var msg = $(".fix-msg", form), heard = form.heard.value.trim(), written = form.written.value.trim();
    var fx = pick && pick.form === form ? dotted(pick.text, pick.a, pick.b) : null;
    var html;
    if (!heard) html = "Select the wrong words in the text above, or type them here.";
    else if (fx && (!written || written === fx.dataset.heard)) html = keepText(fx);
    else if (!written || written === heard) html = "Type how psst should write it.";
    else if (known(written)) html = "<b>" + esc(known(written)) + "</b> is in your dictionary already. This adds “" + esc(heard) + "” as another way Whisper hears it.";
    else html = "Adds <b>" + esc(written) + "</b> to your dictionary. psst writes it from your next dictation.";
    msg.className = "fix-msg";
    msg.innerHTML = html;
    form.written.removeAttribute("aria-invalid");
  }

  // Shows a pick in its dictation's form.
  function fill(keepWritten) {
    var text = pick.text, row = text.closest("[data-row]"), box = $(".fixbox", row), form = $("form.fix", box);
    pick.form = form;
    box.open = true;
    row.classList.add("boxed");
    var heard = heardOf(text, pick.a, pick.b), fx = dotted(text, pick.a, pick.b);
    form.heard.value = heard;
    $("[data-typed]", form).hidden = true;
    var line = $("[data-heard]", form);
    line.hidden = false;
    var before = neighbour(text, pick.a, -1, true), after = neighbour(text, pick.b, 1, true), all = kids(text);
    var grow = function (i, dir) {
      return i < 0 ? "" : '<button class="btn sm" type="button" data-grow="' + dir + '"><svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg><span class="sr">Include </span>' + esc(all[i].firstChild.textContent) + "</button>";
    };
    line.innerHTML = '<p class="heard-line' + (fx ? " why" : "") + '"><span class="label">Whisper heard</span> <span class="hq">“' + esc(heard) + "”</span>" +
      (fx ? "; psst wrote <b>" + esc(fx.dataset.term) + "</b>" + (fx.dataset.how === "fuzzy" ? " because it sounds close." : ".") : "") + "</p>" +
      '<span class="grow">' + grow(before, -1) + grow(after, 1) + "</span>";
    $("[data-keep]", form).hidden = !fx;
    if (!keepWritten) {
      form.written.value = fx ? "" : shownOf(text, pick.a, pick.b);
      form.dataset.prefill = fx ? "" : "1";
    }
    hint(form);
  }
  function choose(text, a, b, anchor, keepWritten) {
    pick = { text: text, a: a, b: b, anchor: anchor == null ? a : anchor, form: null };
    $$(".entry.boxed").forEach(function (el) { if (el !== text.closest(".entry")) el.classList.remove("boxed"); });
    paint();
    fill(keepWritten);
  }
  function focusField() {
    var form = pick && pick.form;
    if (!form) return;
    form.written.focus();
    if (form.dataset.prefill) form.written.select();
  }
  function drop() {
    if (pick && pick.form) {
      var form = pick.form;
      $("[data-typed]", form).hidden = false;
      $("[data-heard]", form).hidden = true;
      $("[data-keep]", form).hidden = true;
    }
    pick = null;
    paint();
  }

  document.addEventListener("click", function (ev) {
    var t = ev.target;
    // The menu closes when you click elsewhere.
    var menu = $("details.menu[open]");
    if (menu && !t.closest("details.menu")) menu.open = false;

    var grow = t.closest("[data-grow]");
    if (grow && pick) {
      var dir = +grow.dataset.grow, typed = pick.form.written.value, wasPrefill = pick.form.dataset.prefill;
      var j = neighbour(pick.text, dir < 0 ? pick.a : pick.b, dir, true);
      if (j >= 0) {
        if (dir < 0) choose(pick.text, j, pick.b, pick.b, !wasPrefill);
        else choose(pick.text, pick.a, j, pick.a, !wasPrefill);
        if (!wasPrefill) pick.form.written.value = typed;
        hint(pick.form);
        var again = $('[data-grow="' + dir + '"]', pick.form);
        (again || pick.form.written).focus();
      }
      return;
    }

    var w = t.closest(".text .w");
    if (!w || ev.detail > 1) return;
    var sel = getSelection();
    if (sel && !sel.isCollapsed) return;
    var text = w.closest(".text"), i = kids(text).indexOf(w), p = pick && pick.text === text ? pick : null;
    var typed = p && p.form && !p.form.dataset.prefill ? p.form.written.value : null;
    if (!p) choose(text, i, i);
    else if (i >= p.a && i <= p.b) {
      if (p.a === p.b) { drop(); return; }
      if (i === p.a) choose(text, neighbour(text, i, 1, false), p.b, p.b, typed != null);
      else if (i === p.b) choose(text, p.a, neighbour(text, i, -1, false), p.a, typed != null);
      else choose(text, i, i);
    } else if (i === neighbour(text, p.a, -1, true)) choose(text, i, p.b, p.b, typed != null);
    else if (i === neighbour(text, p.b, 1, true)) choose(text, p.a, i, p.a, typed != null);
    else choose(text, i, i);
    if (fine() && !dotted(text, pick.a, pick.b)) focusField();
  });

  // Dragging across words picks them; the text stays selected, for copying.
  document.addEventListener("mouseup", function () {
    var sel = getSelection();
    if (!sel || sel.isCollapsed || !sel.rangeCount) return;
    var elOf = function (n) { var e = n && (n.nodeType === 1 ? n : n.parentElement); return e && e.closest(".text > *"); };
    var a = elOf(sel.anchorNode), b = elOf(sel.focusNode);
    if (!a || !b || a.parentElement !== b.parentElement || !a.parentElement.classList.contains("text")) return;
    var text = a.parentElement, all = kids(text), ia = all.indexOf(a), ib = all.indexOf(b);
    var lo = Math.min(ia, ib), hi = Math.max(ia, ib);
    while (lo <= hi && !isWord(all[lo])) lo++;
    while (hi >= lo && !isWord(all[hi])) hi--;
    if (lo > hi) return;
    for (var k = lo; k <= hi; k++) if (all[k].classList.contains("fixed")) return;
    choose(text, lo, hi);
  });

  document.addEventListener("input", function (ev) {
    var form = ev.target.closest && ev.target.closest("form.fix");
    if (form) {
      if (ev.target.name === "written") form.dataset.prefill = "";
      hint(form);
    }
    if (ev.target.matches && ev.target.matches("[data-count-tokens]")) tokens(ev.target);
  });

  // Prefilled words are selected when the field gets focus, so typing replaces them.
  document.addEventListener("focusin", function (ev) {
    var form = ev.target.closest && ev.target.closest("form.fix");
    if (form && ev.target.name === "written" && form.dataset.prefill) setTimeout(function () { ev.target.select(); }, 0);
  });

  document.addEventListener("toggle", function (ev) {
    var box = ev.target;
    if (!box.classList || !box.classList.contains("fixbox")) return;
    var row = box.closest(".entry");
    if (box.open) {
      if (!pick || pick.text.closest(".entry") !== row) {
        drop();
        var heard = $("input[name=heard]", box);
        if (heard && !heard.value) heard.focus();
      }
    } else {
      if (pick && pick.text.closest(".entry") === row) drop();
      if (!$(".done", row)) row.classList.remove("boxed");
    }
  }, true);

  document.addEventListener("keydown", function (ev) {
    var t = ev.target;
    if (ev.key === "Escape") {
      var menu = $("details.menu[open]");
      if (menu) { menu.open = false; $("summary", menu).focus(); return; }
      var close = $(".panel [data-close]");
      if (close) { close.click(); return; }
      if (pick) {
        var text = pick.text, box = pick.form && pick.form.closest(".fixbox");
        drop();
        if (box) box.open = false;
        text.focus();
      }
      return;
    }
    // Enter in a fix field presses the main button.
    if (ev.key === "Enter" && t.closest && t.closest("form.fix") && t.tagName === "INPUT") {
      var form = t.closest("form.fix");
      if (!form.written.value.trim() && pick && dotted(pick.text, pick.a, pick.b)) ev.preventDefault();
      return;
    }
    if (!t.classList || !t.classList.contains("text") || !t.closest("[data-row]") || t.classList.contains("empty-text")) return;
    var all = kids(t), words = all.map(function (el, i) { return isWord(el) ? i : -1; }).filter(function (i) { return i >= 0; });
    if (ev.key === "ArrowUp" || ev.key === "ArrowDown") {
      ev.preventDefault();
      var texts = $$(".text[id^='t-']"), next = texts[texts.indexOf(t) + (ev.key === "ArrowDown" ? 1 : -1)];
      if (next) { t.tabIndex = -1; next.tabIndex = 0; next.focus(); }
      return;
    }
    var p = pick && pick.text === t ? pick : null;
    if (ev.key === "Enter" || ev.key === " ") {
      ev.preventDefault();
      if (p) focusField();
      else { var box = $(".fixbox", t.closest(".entry")); box.open = true; }
      return;
    }
    if (ev.key !== "ArrowLeft" && ev.key !== "ArrowRight") return;
    ev.preventDefault();
    var dir = ev.key === "ArrowRight" ? 1 : -1;
    if (!p) {
      var first = dir > 0 ? words[0] : words[words.length - 1];
      if (first != null) choose(t, first, first);
    } else if (ev.shiftKey) {
      var head = p.anchor === p.a ? p.b : p.a;
      var growing = head === p.anchor || (dir > 0) === (head > p.anchor);
      var moved = neighbour(t, head, dir, growing);
      if (moved >= 0) choose(t, Math.min(p.anchor, moved), Math.max(p.anchor, moved), p.anchor);
    } else {
      var to = neighbour(t, dir > 0 ? p.b : p.a, dir, false);
      if (to >= 0) choose(t, to, to);
    }
    t.focus();
  });

  // After a fix is saved, its confirmation comes into view.
  function arrive() {
    var id = location.hash.slice(1);
    var row = id && document.getElementById(id);
    var done = row && $(".done-h", row);
    if (done) { row.scrollIntoView({ block: "center" }); done.focus({ preventScroll: true }); }
    var refused = $(".fixbox[open] .fix-msg.error, .fixbox[open] .fix-msg.warn");
    if (refused) { var f = refused.closest("form"); f.closest(".entry").scrollIntoView({ block: "center" }); f.written.focus(); }
    var field = $(".panel [autofocus]");
    if (field && fine()) field.focus();
  }
  document.addEventListener("htmx:afterSettle", arrive);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", arrive);
  else arrive();

  // ---------- Profiles: the prompt style's size ----------
  function tokens(area) {
    var n = Math.ceil(new TextEncoder().encode(area.value).length / 4), limit = +area.dataset.countTokens;
    var box = document.getElementById(area.dataset.tokensOut);
    if (!box) return;
    box.classList.toggle("over", n > limit);
    $("span", box).textContent = n + " of " + limit + " tokens";
    $("progress", box).value = Math.min(n, limit);
  }
})();
