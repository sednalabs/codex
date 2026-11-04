export function serviceHeaderPlan(request) {
  const args = request.arguments || {};
  const profiles = serviceProfiles();
  const selected = args.service_profile
    ? profiles.find((profile) => profile.id === args.service_profile)
    : null;
  if (args.service_profile && !selected) {
    throw new Error(`Browser service profile \`${args.service_profile}\` is not configured.`);
  }

  const perCallHeaders = objectEntries(args.extra_http_headers);
  const perCallEnvHeaders = envHeaderEntries(args.extra_http_headers_env);
  if ((perCallHeaders.length > 0 || perCallEnvHeaders.length > 0) && !allowCallHeaders()) {
    throw new Error(
      "Direct browser extra_http_headers require local provider opt-in with allow_call_extra_http_headers.",
    );
  }

  const profileHeaders = selected
    ? [...objectEntries(selected.headers), ...envHeaderEntries(selected.env_headers)]
    : [];
  const headers = [...profileHeaders, ...perCallHeaders, ...perCallEnvHeaders];
  if (headers.length === 0) {
    return { headers: {}, headerNames: [], allowedHosts: [], actor: null, profileId: null };
  }

  const allowedHosts = [
    ...stringArray(selected?.allowed_hosts),
    ...stringArray(args.allowed_hosts),
  ];
  if (allowedHosts.length === 0) {
    throw new Error("Browser service headers require at least one allowed host.");
  }

  return {
    headers: Object.fromEntries(headers),
    headerNames: headers.map(([name]) => name),
    allowedHosts,
    actor: selected?.actor || selected?.id || "service account",
    profileId: selected?.id || null,
  };
}

export async function installServiceHeaderRoute(context, plan) {
  if (!plan || Object.keys(plan.headers).length === 0) {
    return;
  }
  await context.route("**/*", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (!hostAllowed(url.hostname, plan.allowedHosts)) {
      await route.continue();
      return;
    }
    await route.continue({
      headers: {
        ...request.headers(),
        ...lowerCaseHeaderObject(plan.headers),
      },
    });
  });
}

function serviceProfiles() {
  const raw = process.env.CODEX_BROWSER_PLAYWRIGHT_SERVICE_PROFILES_JSON;
  if (!raw || !raw.trim()) {
    return [];
  }
  try {
    const profiles = JSON.parse(raw);
    return Array.isArray(profiles) ? profiles : [];
  } catch {
    return [];
  }
}

function allowCallHeaders() {
  return ["1", "true", "yes", "on"].includes(
    (process.env.CODEX_BROWSER_PLAYWRIGHT_ALLOW_CALL_HEADERS || "").toLowerCase(),
  );
}

function objectEntries(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return [];
  }
  return Object.entries(value)
    .filter(([name, headerValue]) => name && String(headerValue || "").trim())
    .map(([name, headerValue]) => [name, String(headerValue)]);
}

function envHeaderEntries(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return [];
  }
  const entries = [];
  for (const [name, envName] of Object.entries(value)) {
    const headerValue = process.env[String(envName)];
    if (name && headerValue && headerValue.trim()) {
      entries.push([name, headerValue]);
    }
  }
  return entries;
}

function stringArray(value) {
  return Array.isArray(value)
    ? value.filter((item) => typeof item === "string" && item.trim())
    : [];
}

function hostAllowed(host, allowedHosts) {
  return allowedHosts.some((allowed) => {
    if (allowed.startsWith("*.")) {
      const suffix = allowed.slice(1);
      return host.endsWith(suffix);
    }
    return host === allowed;
  });
}

function lowerCaseHeaderObject(headers) {
  return Object.fromEntries(
    Object.entries(headers).map(([name, headerValue]) => [name.toLowerCase(), headerValue]),
  );
}
