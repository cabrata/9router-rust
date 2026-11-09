// Seeds a temp DATA_DIR by running the ORIGINAL 9router JS database layer.
// Spawned by make-js-db-fixture.mjs; not meant to be run directly, because it
// needs the "@/..." resolver hook that script installs.
//
// Paths arrive through env so this file has no generated content.
const jsRoot = process.env.JSDB_9ROUTER_ROOT;
process.env.DATA_DIR = process.env.JSDB_DATA_DIR;

const { getAdapter } = await import(jsRoot + '/src/lib/db/driver.js');
await getAdapter();
const db = await import(jsRoot + '/src/lib/db/index.js');

await db.updateSettings({
  requireApiKey: true,
  stickyRoundRobinLimit: 3,
  cloudEnabled: false,
  enableObservability: true,
});
await db.createProviderConnection({
  provider: 'openai', authType: 'apikey', name: 'Main', priority: 1, isActive: true,
  // Everything but the fixed columns is stringified into the data column.
  apiKey: 'sk-test-123456789',
  providerSpecificData: { baseUrl: 'https://x.test/v1' },
});
await db.createProviderNode({
  // createProviderNode copies id/type/name/prefix/apiType/baseUrl onto the row;
  // anything else is dropped, so the fields go at the top level.
  id: 'node-1', type: 'openai-compatible', name: 'Node',
  baseUrl: 'https://n.test/v1', prefix: 'nc',
});
await db.createProxyPool({
  id: 'pool-1', isActive: true, testStatus: 'active',
  data: { proxyUrl: 'http://127.0.0.1:8080' },
});
const key = await db.createApiKey('Main key', 'machine1234', { limitConcurrent: 2, rpm: 5 });
// updateApiKey reads access from its access field, not from the flat columns.
await db.updateApiKey(key.id, { access: { restricted: true, allow: ['openai/gpt-4o'] } });
await db.createCombo({ name: 'fast', kind: 'fallback', models: ['openai/gpt-4o', 'openai/gpt-4o-mini'] });
await db.setModelAlias('fast', 'openai/gpt-4o-mini');
await db.addCustomModel({ providerAlias: 'openai', id: 'custom-1', type: 'llm', name: 'Custom' });
await db.setMitmAliasAll('cursor', { 'gpt-5': 'openai/gpt-4o' });
await db.updatePricing({ openai: { 'gpt-4o': { input: 2.5, output: 10 } } });
await db.disableModels('openai', ['gpt-3.5']);

// saveRequestDetail buffers and flushes on a timer; give it a moment.
await db.saveRequestDetail({
  id: 'det-1', timestamp: '2026-01-02T00:00:00.000Z', provider: 'openai', model: 'gpt-4o',
  connectionId: 'conn-1', status: 'ok',
  request: { messages: [{ role: 'user', content: 'hi' }] },
});
await new Promise((r) => setTimeout(r, 300));

const { insertRequestLog } = await import(jsRoot + '/src/lib/db/repos/requestLogsRepo.js');
await insertRequestLog({
  timestamp: '2026-01-02T00:00:00.000Z', apiKeyId: key.id, apiKeyName: 'Main key',
  apiKeyMasked: 'sk-live...ijkl', ip: '127.0.0.1', method: 'POST',
  path: '/v1/chat/completions', endpointKind: 'chat', model: 'gpt-4o', provider: 'openai',
  resolvedModel: 'gpt-4o', status: 200, stream: false, promptTokens: 10, completionTokens: 20,
  durationMs: 100, ttftMs: 10, tps: 20.5, userAgent: 'ua',
});

const usage = await import(jsRoot + '/src/lib/usageDb.js');
await usage.saveRequestUsage({
  timestamp: '2026-01-02T00:00:00.000Z', provider: 'openai', model: 'gpt-4o',
  connectionId: 'conn-1', apiKey: key.key, endpoint: '/v1/chat/completions',
  tokens: { prompt_tokens: 10, completion_tokens: 20, total_tokens: 30 }, status: 'ok',
});
