export interface AgentState {
  agentId: string;
  stepIndex: number;
  data: Record<string, unknown>;
}

export class DurableAgent {
  private readonly agentId: string;
  private readonly baseUrl: string;

  constructor(options: { agentId: string; sidecarUrl?: string }) {
    this.agentId = options.agentId;
    this.baseUrl = (options.sidecarUrl ?? "http://localhost:8080").replace(/\/$/, "");
  }

  /**
   * Fetch the last checkpoint on startup.
   * Returns a fresh state (stepIndex=0) if no checkpoint exists.
   * Call this at the start of every agent run to resume after a crash.
   */
  async resume(): Promise<AgentState> {
    const res = await fetch(`${this.baseUrl}/agents/${this.agentId}/state`);
    if (!res.ok) {
      throw new Error(`resume failed: ${res.status} ${await res.text()}`);
    }
    const body = await res.json() as {
      agent_id: string;
      step_index: number;
      state: Record<string, unknown>;
    };
    return {
      agentId: body.agent_id,
      stepIndex: body.step_index,
      data: body.state ?? {},
    };
  }

  /**
   * Persist agent state before a risky operation (LLM call, tool execution).
   * Blocks until the WAL confirms the write — safe to crash after this returns.
   */
  async checkpoint(stepIndex: number, state: Record<string, unknown>): Promise<void> {
    const res = await fetch(`${this.baseUrl}/agents/${this.agentId}/checkpoint`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ step_index: stepIndex, state }),
    });
    if (!res.ok) {
      throw new Error(`checkpoint failed: ${res.status} ${await res.text()}`);
    }
  }

  /**
   * Record an event to the trace log for observability and audit.
   * Returns after the NATS PubAck — TiDB persistence is async.
   */
  async saveEvent(eventId: string, payload: Record<string, unknown>): Promise<void> {
    const res = await fetch(`${this.baseUrl}/agents/${this.agentId}/events`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ event_id: eventId, payload }),
    });
    if (!res.ok) {
      throw new Error(`saveEvent failed: ${res.status} ${await res.text()}`);
    }
  }
}
