import { useEffect, useState } from "react";
import { getAdminToken, getToken } from "./api";
import ChatPage from "./user/ChatPage";
import LoginPage from "./user/LoginPage";
import AdminApp from "./admin/AdminApp";

/** 路由约定：
 *  #/ 或无 hash   → 用户端（访问码登录 + 问答）
 *  #/admin        → 隐藏管理端登录页与控制台（不设入口链接）
 */
export default function App() {
  const [hash, setHash] = useState(window.location.hash);
  useEffect(() => {
    const on = () => setHash(window.location.hash);
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);

  if (hash.startsWith("#/admin")) {
    return <AdminApp />;
  }
  return getToken() ? <ChatPage /> : <LoginPage />;
}

export function useAdminAuthed(): boolean {
  return !!getAdminToken();
}
