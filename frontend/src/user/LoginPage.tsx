import { useState } from "react";
import { api, setAuth } from "../api";
import { ArrowRight, KeyRound, LoaderCircle } from "lucide-react";
import Brand from "../ui/Brand";

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
    <div className="center-page user-login">
      <header className="login-header"><Brand /><span>你的校园政策助手</span></header>
      <form className="login-card" onSubmit={submit}>
        <div className="login-photo"><img src="/images/campus-library.jpg" alt="明亮的图书馆书架" /><span>校园里的事，一起弄明白。</span></div>
        <div className="login-fields"><span className="eyebrow">NICE TO MEET YOU</span><h1>欢迎来到校园知事</h1>
        <p className="muted">班级政策问答</p>
        <label htmlFor="access-code">班级访问码</label><div className="access-field"><KeyRound size={18} />
        <input
          id="access-code"
          type="password"
          placeholder="请输入班级访问码"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          autoFocus
          required
        /></div>
        {error && <div className="error">{error}</div>}
        <button className="primary-button" type="submit" disabled={busy || !code}>
          {busy ? <LoaderCircle size={18} className="spin" /> : <ArrowRight size={18} />}{busy ? "验证中…" : "进入"}
        </button>
        <small className="login-note">访问码由班级管理员提供</small></div>
      </form>
      <footer className="login-footer">CAMPUS NOTES · 有据可循，有问有答</footer>
    </div>
  );
}
