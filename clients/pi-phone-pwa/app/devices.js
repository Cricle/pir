import { getServerBase } from "./config.js";
import { state } from "./state.js";

function authHeaders() {
  return state.token ? { Authorization: `Bearer ${state.token}` } : {};
}

async function readError(response) {
  try {
    const body = await response.json();
    return body?.error?.message || `Request failed (${response.status})`;
  } catch {
    return `Request failed (${response.status})`;
  }
}

export async function refreshDevices() {
  const response = await fetch(`${getServerBase()}/api/devices`, {
    cache: "no-store",
    headers: authHeaders(),
  });
  if (!response.ok) throw new Error(await readError(response));
  const data = await response.json();
  state.devices = Array.isArray(data.devices) ? data.devices : [];
  return state.devices;
}

export async function createDeviceToken(name) {
  const response = await fetch(`${getServerBase()}/api/devices`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({
      name,
      platform: typeof navigator !== "undefined" ? navigator.platform || "web" : "web",
    }),
  });
  if (!response.ok) throw new Error(await readError(response));
  const data = await response.json();
  state.lastDeviceToken = data.token || null;
  await refreshDevices();
  return data;
}

export async function revokeDevice(deviceId) {
  const response = await fetch(`${getServerBase()}/api/devices/${encodeURIComponent(deviceId)}`, {
    method: "DELETE",
    headers: authHeaders(),
  });
  if (!response.ok) throw new Error(await readError(response));
  await refreshDevices();
}
