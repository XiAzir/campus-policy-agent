import { useState } from "react";
import { api, setAuth } from "../api";

export default function LoginPage() {
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      const r = await api.login(code);
      setAuth(r.token, r.client_id);
      window.location.reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : "登录失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="center-page">
      <form className="login-card" onSubmit={submit}>
        <h1>班级政策问答</h1>
        <p className="muted">输入班级访问码进入；如无访问码请联系管理员。</p>
        <input
          type="password"
          placeholder="访问码"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          autoFocus
          required
        />
        {error && <div className="error">{error}</div>}
        <button type="submit" disabled={busy || !code}>
          {busy ? "验证中…" : "进入"}
        </button>
      </form>
    </div>
  );
}
