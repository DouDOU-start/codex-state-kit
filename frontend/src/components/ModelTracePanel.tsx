import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { t, formatNumber } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import { runModelTraceProbe } from "@/lib/api";
import { Select } from "@/components/Select";
import type { ModelTraceAttempt, Status } from "@/types";
import {
  MODELTRACE_MAX_ATTEMPTS,
  MODELTRACE_BANK_SHA256,
  MODELTRACE_MODEL_COUNT,
  MODELTRACE_SCORER_SHA256,
  MODELTRACE_TARGET_OUTPUTS,
  analyzeModelTraceOutputs,
  appendModelTraceHistory,
  generateModelTraceChallenges,
  outputDiagnostic,
  readModelTraceHistory,
  type ModelTraceHistoryEntry,
} from "@/modeltrace/modeltrace";
import type { ModelTraceAnalysisResult } from "@/modeltrace/fingerprint-core.js";

interface ModelTracePanelProps {
  active: boolean;
  status: Status;
}

function percent(value: number): string {
  return `${(value * 100).toFixed(1)}%`;
}

function probabilityPercent(value: number): string {
  if (value > 0 && value < 0.001) return "<0.1%";
  return percent(value);
}

function diagnosticMessage(message: string | null): string {
  if (!message) return t("有效");
  if (message === "探测请求失败") return t("探测请求失败");
  if (message === "有效数字不足，响应可能被截断或拒答") return t("有效数字不足，响应可能被截断或拒答");
  return message;
}

const CODEX_CLI_MODELS = [
  "gpt-6-astra",
  "gpt-6-sol",
  "gpt-6-luna",
  "gpt-5.6-sol",
  "gpt-5.6-terra",
  "gpt-5.6-luna",
];

function defaultModel(status: Status): string {
  return status.forcedModel
    || status.logs.find((entry) => entry.model)?.model
    || CODEX_CLI_MODELS[0];
}

function historyEntry(
  status: Status,
  requestedModel: string,
  sentModel: string,
  attempts: ModelTraceAttempt[],
  analysis: ModelTraceAnalysisResult,
): ModelTraceHistoryEntry {
  return {
    id: `modeltrace-${Date.now()}-${Math.random().toString(16).slice(2)}`,
    at: new Date().toISOString(),
    accountId: status.currentAccountId ?? null,
    accountEmail: status.currentAccountEmail ?? null,
    requestedModel,
    sentModel,
    outboundMode: status.outboundMode,
    outboundNode: status.mihomoNode || status.mihomo?.selected || null,
    attempts: attempts.map(outputDiagnostic),
    analysis,
  };
}

function Result({ analysis }: { analysis: ModelTraceAnalysisResult }) {
  return (
    <div className="modeltrace-result">
      <div className="modeltrace-result__summary">
        <div><span>{t("最可能模型")}</span><strong>{analysis.prediction_name}</strong></div>
        <div><span>{t("模型概率")}</span><strong>{percent(analysis.probability)}</strong></div>
        <div><span>{t("模型家族")}</span><strong>{analysis.family_prediction_name} · {percent(analysis.family_probability)}</strong></div>
        <div><span>{t("有效挑战")}</span><strong>{analysis.used_outputs}/{MODELTRACE_TARGET_OUTPUTS}</strong></div>
      </div>
      <div className="modeltrace-table-wrap">
        <table className="modeltrace-table">
          <thead><tr><th>#</th><th>{t("候选模型")}</th><th>{t("家族")}</th><th>{t("归因概率")}</th><th>{t("分布相似度")}</th></tr></thead>
          <tbody>
            {analysis.results.map((item, index) => (
              <tr key={item.model} className={index === 0 ? "modeltrace-table__winner" : undefined}>
                <td>{index + 1}</td>
                <td><strong>{item.display_name}</strong><small>{item.model}</small></td>
                <td>{item.family_name}</td>
                <td><div className="modeltrace-probability" title={`${(item.probability * 100).toFixed(4)}%`}><span><i style={{ width: `${item.probability * 100}%` }} /></span><strong>{probabilityPercent(item.probability)}</strong></div></td>
                <td>{percent(item.profile_similarity)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="modeltrace-families">
        {analysis.family_probabilities.map((family) => (
          <span key={family.family}><strong>{family.display_name}</strong><b>{percent(family.probability)}</b></span>
        ))}
      </div>
      <p className="modeltrace-note">{t("概率来自当前 ModelTrace 指纹库的闭集校准，仅表示候选库内的相对归因；未收录模型也可能被归入最相似候选。")}</p>
      <p className="modeltrace-note">{t("校准参数：{0} 个回答 · β={1} · 交叉验证准确率 {2}", [analysis.calibration.queries, analysis.calibration.beta.toFixed(3), percent(analysis.calibration.cv_accuracy)])}</p>
      <p className="modeltrace-note">{t("评分器版本 {0} · 指纹库 {1} · {2} 个候选模型", [MODELTRACE_SCORER_SHA256.slice(0, 12), MODELTRACE_BANK_SHA256.slice(0, 12), MODELTRACE_MODEL_COUNT])}</p>
    </div>
  );
}

export function ModelTracePanel({ active, status }: ModelTracePanelProps) {
  useLocale();
  const [model, setModel] = useState("");
  const [reasoningEffort, setReasoningEffort] = useState(status.forcedReasoningEffort || "low");
  const [attempts, setAttempts] = useState<ModelTraceAttempt[]>([]);
  const [analysis, setAnalysis] = useState<ModelTraceAnalysisResult | null>(null);
  const [history, setHistory] = useState<ModelTraceHistoryEntry[]>([]);
  const [running, setRunning] = useState(false);
  const [cancelled, setCancelled] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancelRef = useRef(false);
  const runTokenRef = useRef(0);

  useEffect(() => {
    if (!active) return;
    setHistory(readModelTraceHistory());
    setModel((current) => current || defaultModel(status));
  }, [active, status.currentAccountId, status.currentAccountEmail, status.forcedModel]);

  useEffect(() => {
    setReasoningEffort(status.forcedReasoningEffort || "low");
  }, [status.forcedReasoningEffort]);

  const modelSelectOptions = useMemo(
    () => {
      const values = new Set(CODEX_CLI_MODELS);
      if (model) values.add(model);
      return [...values].map((value) => ({ value, label: value }));
    },
    [model],
  );

  const cancel = useCallback(() => {
    cancelRef.current = true;
    runTokenRef.current += 1;
    setCancelled(true);
    setAnalysis(null);
    setRunning(false);
  }, []);

  const run = useCallback(async () => {
    const requestedModel = model.trim();
    if (!requestedModel) {
      setError(t("请输入要探测的模型 ID"));
      return;
    }
    if (!status.currentAccountId) {
      setError(t("请先登录 ChatGPT 账号"));
      return;
    }
    cancelRef.current = false;
    const runToken = ++runTokenRef.current;
    setCancelled(false);
    setRunning(true);
    setError(null);
    setAnalysis(null);
    setAttempts([]);
    const challenges = generateModelTraceChallenges();
    const collected: ModelTraceAttempt[] = [];
    try {
      for (const challenge of challenges) {
        if (cancelRef.current) break;
        const attempt = await runModelTraceProbe(requestedModel, reasoningEffort, challenge);
        if (runToken !== runTokenRef.current) return;
        collected.push(attempt);
        setAttempts([...collected]);
        const valid = collected.filter((item) => outputDiagnostic(item).accepted).length;
        if (valid >= MODELTRACE_TARGET_OUTPUTS) break;
      }
      const valid = collected.filter((item) => outputDiagnostic(item).accepted);
      if (cancelRef.current) return;
      if (!valid.length) throw new Error(t("没有获得可分析的有效数字序列"));
      const nextAnalysis = analyzeModelTraceOutputs(valid);
      setAnalysis(nextAnalysis);
      const sentModel = valid[0]?.sentModel || requestedModel;
      const entry = historyEntry(status, requestedModel, sentModel, collected, nextAnalysis);
      setHistory(appendModelTraceHistory(entry));
    } catch (cause) {
      if (runToken !== runTokenRef.current) return;
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (runToken === runTokenRef.current) setRunning(false);
    }
  }, [model, reasoningEffort, status]);

  return (
    <section className="panel modeltrace-panel">
      <header>
        <div className="section-heading">
          <span className="section-icon">◎</span>
          <div><h2>{t("模型归因")}</h2><p>{t("用 ModelTrace 主动探测当前账号返回的模型指纹")}</p></div>
        </div>
        <span className="section-step">{t("最多 6 次 · 3 份有效回答")}</span>
      </header>
      <p className="panel__hint">{t("探测会经当前账号、虚拟设备和出站线路发送最多 6 次请求，目标是收集 3 份有效数字序列。探测请求不计入业务账单，但会消耗上游额度。")}</p>
      <div className="modeltrace-controls">
        <label className="field">
          <span>{t("目标模型")}</span>
          <Select
            ariaLabel={t("目标模型")}
            value={model}
            disabled={running}
            options={modelSelectOptions}
            placeholder="gpt-6-astra"
            onChange={setModel}
          />
        </label>
        <div className="field">
          <span>{t("思考等级")} <small>{t("可选")}</small></span>
          <Select
            ariaLabel={t("思考等级")}
            value={reasoningEffort}
            disabled={running}
            options={[
              { value: "", label: t("跟随当前设置") },
              { value: "low", label: "low" },
              { value: "medium", label: "medium" },
              { value: "high", label: "high" },
              { value: "xhigh", label: "xhigh" },
              { value: "max", label: "max" },
            ]}
            onChange={setReasoningEffort}
          />
        </div>
      </div>
      <div className="panel__actions">
        <button className="button button--primary" type="button" disabled={running || !status.currentAccountId} onClick={() => void run()}>{running ? <><i className="modeltrace-spinner" aria-hidden="true" />{t("探测中…")}</> : t("开始归因探测")}</button>
        {running ? <button className="button button--ghost" type="button" onClick={cancel}>{t("取消")}</button> : null}
        <span className="modeltrace-account">{status.currentAccountEmail || status.currentAccountId || t("尚未登录")}</span>
      </div>
      {running || attempts.length ? (
        <div className={`modeltrace-progress${running ? " is-running" : ""}`} aria-live="polite">
          <strong>{cancelled ? t("已取消") : t("探测进度")}</strong>
          <span>{attempts.filter((item) => outputDiagnostic(item).accepted).length}/{MODELTRACE_TARGET_OUTPUTS} {t("份有效回答")} · {attempts.length}/{MODELTRACE_MAX_ATTEMPTS} {t("次尝试")}</span>
          <div className="modeltrace-attempts">{attempts.map((attempt) => {
            const diagnostic = outputDiagnostic(attempt);
            return <span key={attempt.challengeId} className={diagnostic.accepted ? "is-ok" : "is-error"} title={diagnostic.error ?? "ok"}>{diagnostic.accepted ? `✓ ${diagnostic.parsedNumbers}` : `× ${diagnostic.parsedNumbers}`}</span>;
          })}</div>
          <div className="modeltrace-diagnostics">
            {attempts.map((attempt, index) => {
              const diagnostic = outputDiagnostic(attempt);
              return <span key={`${attempt.challengeId}-detail`}>{index + 1}. {diagnosticMessage(diagnostic.error)}</span>;
            })}
          </div>
        </div>
      ) : null}
      {error ? <p className="modeltrace-error" role="alert">{error}</p> : null}
      {analysis ? <Result analysis={analysis} /> : null}
      {history.length ? (
        <div className="modeltrace-history">
          <div className="modeltrace-history__heading"><strong>{t("归因历史")}</strong><span>{t("最近 {0} 次", [history.length])}</span></div>
          {history.slice(0, 8).map((entry) => (
            <button key={entry.id} type="button" className="modeltrace-history__row" onClick={() => { setAnalysis(entry.analysis); setAttempts([]); setError(null); }}>
              <span className="modeltrace-history__date">{new Date(entry.at).toLocaleString()}</span>
              <span className="modeltrace-history__field"><small>{t("目标模型")}</small><strong>{entry.requestedModel}</strong></span>
              <span className="modeltrace-history__field"><small>{t("最可能模型")}</small><strong>{entry.analysis.prediction_name}</strong></span>
              <b className="modeltrace-history__probability">{percent(entry.analysis.probability)}</b>
            </button>
          ))}
        </div>
      ) : null}
      <p className="modeltrace-source">{t("指纹库固定随应用打包；原始算法与来源见 ModelTrace。")}</p>
    </section>
  );
}
