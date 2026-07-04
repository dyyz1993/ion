/**
 * 全局 state —— 简单 mutable 对象，不用 Redux
 *
 * 改 state 后调 rerender() 重建整树。
 * Input 的值也存这里（drafts），避免 VNode 重建后丢值。
 */

export type Worker = {
  worker_id: string;
  session_id: string;
  project: string;
  status: "idle" | "busy" | "stale" | "dead" | string;
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

export type Panel = "tree" | "kanban" | "detail" | "input";

export const state = {
  // 数据
  workers: [] as Worker[],
  projects: [] as Project[],
  totalStale: 0,

  // UI 状态
  selectedSessionId: null as string | null,
  focusMode: false,
  collapsed: new Set<string>(),
  drafts: new Map<string, string>(),
  createModal: null as CreateModal | null,
  focusedPanel: "tree" as Panel,

  // 输入框（普通模式底部 + Focus 模式内）
  inputValue: "",
  inputCursor: 0,

  // 连接
  connected: false,
};

/** rerender hook —— 由 index.ts 注册 */
let rerenderFn: (() => void) | null = null;

export function setRerenderFn(fn: () => void) {
  rerenderFn = fn;
}

export function rerender() {
  if (rerenderFn) rerenderFn();
}

/** 切换 session，保存/恢复草稿 */
export function switchSession(newSid: string | null) {
  // 保存当前草稿
  if (state.selectedSessionId && state.inputValue) {
    state.drafts.set(state.selectedSessionId, state.inputValue);
  }
  state.selectedSessionId = newSid;
  // 恢复新草稿
  state.inputValue = newSid ? state.drafts.get(newSid) ?? "" : "";
  state.inputCursor = state.inputValue.length;
}
