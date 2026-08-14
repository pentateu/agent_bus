// The fetch wrapper: base URL + in-memory bearer token (§2.3). The token is
// read from the URL hash by `bootstrapToken` and kept in a module-scope
// variable — never persisted.

let token: string | null = null;

export function setToken(t: string | null) {
  token = t;
}

export function hasToken(): boolean {
  return token !== null;
}

/** Read `#token=<t>` from the URL, keep it in memory, strip it from the URL. */
export function bootstrapToken(): string | null {
  const hash = window.location.hash;
  const m = /#token=([^&]+)/.exec(hash);
  if (!m) return token;
  const t = decodeURIComponent(m[1]);
  setToken(t);
  window.history.replaceState(null, "", window.location.pathname + window.location.search);
  return t;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = { ...(init?.headers as Record<string, string>) };
  if (token) headers.Authorization = `Bearer ${token}`;
  const res = await fetch(path, { ...init, headers });
  if (res.status === 401) throw new ApiError(401, "unauthorized — run `supervisor web`");
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new ApiError(res.status, body || res.statusText);
  }
  return (await res.json()) as T;
}

export const get = <T>(path: string) => request<T>(path);
export const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
export const put = <T>(path: string, body: unknown) =>
  request<T>(path, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
