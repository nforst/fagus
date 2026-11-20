/* Renders a paragraph with the C library and writes the PDF to the file of its argument (the
 * test `works_from_c` builds and runs it). */
#include <stdio.h>
#include <string.h>

#include "fagus.h"

int main(int argc, char **argv) {
    if (argc != 2 || fagus_abi() != 0 || fagus_protocol() != 1) {
        return 2;
    }
    char *error = NULL;
    FagusEngine *engine = fagus_engine_new(NULL, 0, &error);
    if (engine == NULL) {
        fprintf(stderr, "%s\n", error);
        fagus_string_free(error);
        return 1;
    }
    const char *html = "<p>Hallo aus C</p>";
    char request[128];
    snprintf(request, sizeof request, "{\"id\":1,\"html\":0,\"blobs\":[%zu]}", strlen(html));
    FagusResult *result = fagus_render(engine, request, strlen(request), html, strlen(html));
    size_t length = 0;
    const char *header = fagus_result_header(result, &length);
    if (!fagus_result_ok(result)) {
        fprintf(stderr, "%s\n", header);
        fagus_result_free(result);
        fagus_engine_free(engine);
        return 1;
    }
    const uint8_t *pdf = fagus_result_pdf(result, &length);
    FILE *file = fopen(argv[1], "wb");
    if (file == NULL || fwrite(pdf, 1, length, file) != length || fclose(file) != 0) {
        return 1;
    }
    printf("%s\n", strstr(header, "\"pages\":1") ? "1 page" : header);
    fagus_result_free(result);
    fagus_engine_free(engine);
    return 0;
}
