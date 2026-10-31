# draw.io PNG fixtures

두 PNG는 합성 1×1 이미지다. `drawio-valid.png`는 `mxfile` 키의 `tEXt` chunk에 URI 인코딩한 XML을 포함한다. `drawio-invalid.png`는 같은 위치에 닫히지 않은 XML을 넣었다. chunk CRC는 정상이다.

정상 XML의 한글 label과 PNG 픽셀은 서로의 시각적 대응을 검증하려는 자료가 아니다. 내장 데이터 판정, 이미지 fallback과 원본 bytes 보존을 검사한다. 저장 형식은 [draw.io의 XML PNG export 구현](https://github.com/jgraph/drawio/blob/dev/src/main/webapp/js/diagramly/EditorUi.js)을 참고했다. 실제 편집기에서 export하거나 다시 연 파일은 아니다.
