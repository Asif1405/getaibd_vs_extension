import { SECRET_RULES } from "./rules";

export interface Detection {
  ruleId: string;
  label: string;
  match: string;
  start: number;
  end: number;
  confidence: number;
}

export function scanText(text: string, minConfidence = 0.7): Detection[] {
  const results: Detection[] = [];
  const seen = new Set<string>();

  for (const rule of SECRET_RULES) {
    if (rule.minConfidence < minConfidence) {continue;}

    const regex = new RegExp(rule.pattern.source, "g");
    let m: RegExpExecArray | null;

    while ((m = regex.exec(text)) !== null) {
      const key = `${rule.id}:${m.index}`;
      if (seen.has(key)) {continue;}
      seen.add(key);

      results.push({
        ruleId: rule.id,
        label: rule.label,
        match: m[0].slice(0, 12) + "..." + m[0].slice(-4),
        start: m.index,
        end: m.index + m[0].length,
        confidence: rule.minConfidence,
      });
    }
  }

  return results;
}

export function containsSecrets(text: string): boolean {
  return SECRET_RULES.some((rule) => rule.pattern.test(text));
}

export function formatWarning(detections: Detection[]): string {
  if (detections.length === 0) {return "";}
  const items = detections.map((d) => `  - ${d.label}: ${d.match}`).join("\n");
  return `Potential secrets detected:\n${items}`;
}
