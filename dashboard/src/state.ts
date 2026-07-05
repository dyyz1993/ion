/**
 * 全局状态
 *
 * shared 是 mutable 对象（网络轮询直接改），
 * refreshCounter 递增触发 Ink 重新渲染。
 */
import { useState, useEffect, useCallback } from "react";

export type Worker = {
  worker_id: string;
  session_id: string;
  project: string;
  status: string;
  model: string;
  model_size?: string | null;
  agent: string;
  channels: string[];
  parent: string | null;
  children: string[];
  latest_output?: string[];
  log_short?: string | null;
  started_at?: number;
};

export type Project = {
  name: string;
  path: string;
  worker_count: number;
};

export type CreateModal = {
  field: "path" | "agent";
  path: string;
  agent: string;
  error?: string;
};

export type ChatMessage = {
  role: "user" | "assistant" | "system";
  content: string;
  streaming?: boolean;
};

// 全局 mutable state
export const S = {
  workers: [] as Worker[],
  projects: [] as Project[],
  totalStale: 0,
  selectedSessionId: null as string | null,
  focusMode: false,
  focusedPanel: "tree" as string,
  inputValue: "",
  createModal: null as CreateModal | null,
  connected: false,
  messages: new Map<string, ChatMessage[]>(),
  logs: [] as string[],
};

let logCount = 0;
export function log(msg: string) {
  S.logs.unshift(`[${new Date().toLocaleTimeString()}] ${msg}`);
  if (S.logs.length > 50) S.logs.length = 50;
  logCount++;
}

/** 全局 render 触发器 */
let triggerRender: (() => void) | null = null;

/** App 调用一次，拿到 trigger 函数 */
export function useRefresh() {
  const [, setTick] = useState(0);
  const trigger = useCallback(() => setTick((n) => n + 1), []);
  useEffect(() => { triggerRender = trigger; return () => { triggerRender = null; }; }, [trigger]);
  return trigger;
}

/** 外部（轮询/网络）调用来触发重渲染 */
export function refresh() {
  // 在 Ink 渲染循环外调用需要小心，用 setTimeout 避免 React 批次警告
  setTimeout(() => triggerRender?.(), 0);
}
