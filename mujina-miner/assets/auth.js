// Optional API token for writes. Loaded first on every page. When the
// device has a token configured, POST/PATCH/PUT/DELETE need
// "Authorization: Bearer <token>"; reads stay open. The token is asked
// for once on the first 401 and kept in this browser.
(function () {
  "use strict";
  var KEY = "mujina-api-token";
  var ASK = "This device requires an API token for changes. Enter it:";

  function getToken() {
    try { return localStorage.getItem(KEY); } catch (e) { return null; }
  }
  function setToken(t) {
    try { localStorage.setItem(KEY, t); } catch (e) { /* not persisted */ }
  }
  function isWrite(method) {
    var m = String(method || "GET").toUpperCase();
    return m !== "GET" && m !== "HEAD" && m !== "OPTIONS";
  }
  function sameOrigin(url) {
    try { return new URL(url, location.href).origin === location.origin; } catch (e) { return false; }
  }
  function askToken() {
    var t = window.prompt(ASK);
    t = t && t.trim();
    if (t) setToken(t);
    return t || null;
  }
  function withAuth(init, token) {
    var headers = new Headers(init.headers || {});
    headers.set("Authorization", "Bearer " + token);
    return Object.assign({}, init, { headers: headers });
  }

  var origFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    init = init || {};
    var url = typeof input === "string" ? input : input.url;
    var method = init.method || (typeof input === "string" ? "GET" : input.method);
    if (!isWrite(method) || !sameOrigin(url)) return origFetch(input, init);
    var token = getToken();
    return origFetch(input, token ? withAuth(init, token) : init).then(function (r) {
      if (r.status !== 401) return r;
      var fresh = askToken();
      return fresh ? origFetch(input, withAuth(init, fresh)) : r; // one prompt, one retry
    });
  };

  var origOpen = XMLHttpRequest.prototype.open;
  var origSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (method, url) {
    this._mujinaWrite = isWrite(method) && sameOrigin(url);
    return origOpen.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    if (this._mujinaWrite) {
      var token = getToken();
      if (token) this.setRequestHeader("Authorization", "Bearer " + token);
      this.addEventListener("loadend", function () {
        // An upload is not replayed automatically: save the token and ask for a retry.
        if (this.status === 401 && askToken()) alert("Token saved. Please retry.");
      });
    }
    return origSend.apply(this, arguments);
  };

  window.mujinaClearToken = function () {
    try { localStorage.removeItem(KEY); } catch (e) { /* nothing stored */ }
  };
})();
