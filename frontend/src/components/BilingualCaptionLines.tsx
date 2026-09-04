import React from 'react';

export type CaptionTranslationState = 'idle' | 'pending' | 'ready' | 'error';

export interface BilingualCaptionLine {
  id: string;
  original: string;
  translation?: string;
  translationState: CaptionTranslationState;
}

interface BilingualCaptionLinesProps {
  lines: readonly BilingualCaptionLine[];
  fontSize: number;
}

export function BilingualCaptionLines({ lines, fontSize }: BilingualCaptionLinesProps) {
  return (
    <div className="flex w-full flex-col gap-2.5" data-testid="bilingual-caption-lines">
      {lines.map((line, index) => {
        const latest = index === lines.length - 1;
        const originalSize = latest ? fontSize : Math.max(14, Math.round(fontSize * 0.72));
        const translatedSize = Math.max(13, Math.round(originalSize * 0.76));
        return (
          <div key={line.id} className={latest ? 'opacity-100' : 'opacity-65'}>
            <p
              className={`line-clamp-2 leading-[1.12] text-white ${latest ? 'font-semibold' : 'font-medium'}`}
              style={{
                fontSize: `${originalSize}px`,
                textShadow: '0 2px 9px rgba(0, 0, 0, 0.95)',
              }}
            >
              {line.original}
            </p>

            {line.translationState !== 'idle' && (
              <div className="mx-auto mt-1.5 flex w-fit max-w-full items-start justify-center gap-2">
                <span className="mt-[0.28em] h-[1em] w-0.5 shrink-0 rounded-full bg-[#6EE7D8]/80" aria-hidden="true" />
                {line.translationState === 'ready' ? (
                  <p
                    className="line-clamp-2 text-balance font-medium leading-[1.2] text-[#A7F3E8]"
                    style={{
                      fontSize: `${translatedSize}px`,
                      textShadow: '0 2px 8px rgba(0, 0, 0, 0.9)',
                    }}
                  >
                    {line.translation}
                  </p>
                ) : line.translationState === 'pending' ? (
                  <p className="text-white/42" style={{ fontSize: `${Math.max(12, translatedSize - 1)}px` }}>
                    正在翻译…
                  </p>
                ) : (
                  <p
                    className="line-clamp-1 text-amber-200/80"
                    role="status"
                    style={{ fontSize: `${Math.max(12, translatedSize - 1)}px` }}
                  >
                    译文暂不可用，请检查翻译设置
                  </p>
                )}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
