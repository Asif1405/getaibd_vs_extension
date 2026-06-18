import * as https from 'https';

const BASE_URL = 'getaibd.com';

/**
 * Model type returned from the API
 */
export interface Model {
  id: string;
  name: string;
}

/**
 * Make an HTTPS request and return the raw response body as a string.
 */
function request(
  method: string,
  path: string,
  apiKey: string,
  body?: string
): Promise<string> {
  return new Promise((resolve, reject) => {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${apiKey}`,
    };

    if (body) {
      headers['Content-Length'] = Buffer.byteLength(body).toString();
    }

    const options: https.RequestOptions = {
      hostname: BASE_URL,
      port: 443,
      path,
      method,
      headers,
    };

    const req = https.request(options, (res) => {
      let data = '';
      res.on('data', (chunk: Buffer) => {
        data += chunk.toString();
      });
      res.on('end', () => {
        if (res.statusCode && res.statusCode >= 200 && res.statusCode < 300) {
          resolve(data);
        } else {
          reject(
            new Error(
              `API request failed with status ${res.statusCode}: ${data}`
            )
          );
        }
      });
    });

    req.on('error', (err) => {
      reject(new Error(`Network error: ${err.message}`));
    });

    if (body) {
      req.write(body);
    }

    req.end();
  });
}

/**
 * Fetch the list of available models from GetAIBD.
 * GET https://getaibd.com/v1/api/models
 */
export async function fetchModels(apiKey: string): Promise<Model[]> {
  if (!apiKey) {
    return [];
  }

  try {
    const raw = await request('GET', '/v1/api/models', apiKey);
    const parsed = JSON.parse(raw);

    // Handle various possible response shapes:
    // 1. Direct array: [{ id, name }, ...]
    // 2. Wrapped: { models: [...] } or { data: [...] }
    let models: unknown[];
    if (Array.isArray(parsed)) {
      models = parsed;
    } else if (parsed.models && Array.isArray(parsed.models)) {
      models = parsed.models;
    } else if (parsed.data && Array.isArray(parsed.data)) {
      models = parsed.data;
    } else {
      console.warn('Unexpected models response shape:', parsed);
      return [];
    }

    return models.map((m: unknown) => {
      const model = m as Record<string, unknown>;
      return {
        id: String(model.id ?? model.model ?? model.name ?? ''),
        name: String(model.name ?? model.id ?? model.model ?? 'Unknown'),
      };
    });
  } catch (err) {
    console.error('Failed to fetch models:', err);
    throw err;
  }
}

/**
 * Generate a response using the selected model.
 * POST https://getaibd.com/v1/api/generate
 */
export async function generateResponse(
  apiKey: string,
  model: string,
  prompt: string
): Promise<string> {
  if (!apiKey) {
    throw new Error('API key is not configured. Please set it in Settings.');
  }
  if (!model) {
    throw new Error('No model selected. Please select a model first.');
  }

  const body = JSON.stringify({ model, prompt });

  try {
    const raw = await request('POST', '/v1/api/generate', apiKey, body);
    const parsed = JSON.parse(raw);

    // Handle various possible response shapes:
    // 1. { response: "..." }
    // 2. { data: { response: "..." } }
    // 3. { choices: [{ message: { content: "..." } }] }
    // 4. { result: "..." }
    // 5. { content: "..." }
    if (typeof parsed === 'string') {
      return parsed;
    } else if (parsed.text) {
      return String(parsed.text);
    } else if (parsed.response) {
      return String(parsed.response);
    } else if (parsed.result) {
      return String(parsed.result);
    } else if (parsed.content) {
      return String(parsed.content);
    } else if (parsed.data?.response) {
      return String(parsed.data.response);
    } else if (parsed.choices?.[0]?.message?.content) {
      return String(parsed.choices[0].message.content);
    } else if (parsed.choices?.[0]?.text) {
      return String(parsed.choices[0].text);
    } else {
      // Fallback: return the entire response as formatted JSON
      return JSON.stringify(parsed, null, 2);
    }
  } catch (err) {
    console.error('Failed to generate response:', err);
    throw err;
  }
}
