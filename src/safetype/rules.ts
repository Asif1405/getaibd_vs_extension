export interface SecretRule {
  id: string;
  label: string;
  pattern: RegExp;
  minConfidence: number;
}

export const SECRET_RULES: SecretRule[] = [
  { id: "openai", label: "OpenAI API Key", pattern: /sk-[a-zA-Z0-9]{32,}/, minConfidence: 0.95 },
  { id: "openai_proj", label: "OpenAI Project Key", pattern: /sk-proj-[a-zA-Z0-9_-]{32,}/, minConfidence: 0.95 },
  { id: "anthropic", label: "Anthropic API Key", pattern: /sk-ant-[a-zA-Z0-9-]{32,}/, minConfidence: 0.95 },
  { id: "gemini", label: "Google Gemini API Key", pattern: /AIza[a-zA-Z0-9_-]{35}/, minConfidence: 0.9 },
  { id: "huggingface", label: "HuggingFace API Key", pattern: /hf_[a-zA-Z0-9]{34}/, minConfidence: 0.95 },
  { id: "replicate", label: "Replicate API Token", pattern: /r8_[a-zA-Z0-9]{40}/, minConfidence: 0.95 },
  { id: "runpod", label: "RunPod API Key", pattern: /rpa_[a-zA-Z0-9]{14,}/, minConfidence: 0.9 },
  { id: "xai", label: "xAI (Grok) API Key", pattern: /xai-[a-zA-Z0-9]{32,}/, minConfidence: 0.95 },
  { id: "deepseek", label: "DeepSeek API Key", pattern: /sk-[a-f0-9]{48,}/, minConfidence: 0.7 },
  { id: "openrouter", label: "OpenRouter API Key", pattern: /sk-or-v1-[a-zA-Z0-9]{48,}/, minConfidence: 0.95 },
  { id: "aws_access", label: "AWS Access Key", pattern: /AKIA[0-9A-Z]{16}/, minConfidence: 0.95 },
  { id: "aws_secret", label: "AWS Secret Key", pattern: /(?:aws_secret_access_key|secret_key)\s*[=:]\s*["']?[A-Za-z0-9/+=]{40}/, minConfidence: 0.85 },
  { id: "jwt", label: "JSON Web Token", pattern: /eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/, minConfidence: 0.8 },
  { id: "private_key", label: "Private Key", pattern: /-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----/, minConfidence: 0.99 },
  { id: "github_pat", label: "GitHub Personal Access Token", pattern: /ghp_[a-zA-Z0-9]{36}/, minConfidence: 0.95 },
  { id: "github_fine", label: "GitHub Fine-Grained Token", pattern: /github_pat_[a-zA-Z0-9]{22}_[a-zA-Z0-9]{59}/, minConfidence: 0.95 },
  { id: "slack_token", label: "Slack Token", pattern: /xox[bporas]-[a-zA-Z0-9-]{10,}/, minConfidence: 0.9 },
  { id: "discord_token", label: "Discord Bot Token", pattern: /[MN][A-Za-z0-9]{23,}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}/, minConfidence: 0.85 },
];
