export interface ModelTraceAnalysisResult {
  prediction: string;
  prediction_name: string;
  probability: number;
  used_outputs: number;
  results: Array<{
    model: string;
    display_name: string;
    probability: number;
    profile_similarity: number;
    score: number;
    family: string;
    family_name: string;
    conditional_probability: number;
  }>;
  diagnostics: Array<{
    index: number;
    parsed_numbers: number;
    minimum_numbers: number;
    accepted: boolean;
  }>;
  calibration: { queries: string; beta: number; cv_accuracy: number };
  family_prediction: string;
  family_prediction_name: string;
  family_probability: number;
  family_probabilities: Array<{ family: string; display_name: string; probability: number }>;
  method: string;
}

export function parseNumbers(text: string): number[];
export function analyzeGlobalOutputs(
  outputs: Array<{ text: string; expected_count: number }>,
  bank: unknown,
): ModelTraceAnalysisResult;
