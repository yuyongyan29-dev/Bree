"""Shared VT cell recorder, extracted from theme-check.py without its runner."""
import codecs
import re
import unicodedata

CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")


PIXEL_RGB = {(40, 40, 40), (77, 77, 77), (134, 134, 132),
             (185, 183, 176), (222, 220, 213), (249, 246, 239)}


PIXEL_INDEXED = {236, 239, 244, 250, 253, 231}


HEADER_MIN_COLUMNS = 60


HEADER_MIN_ROWS = 22


MASCOT_MIN_ROWS = 26


MASCOT_WIDTH = 20


MASCOT_HEIGHT = 10


def pixel_color(color):
    return (color is not None and
            ((color[0] == "rgb" and color[1:] in PIXEL_RGB) or
             (color[0] == "indexed" and color[1] in PIXEL_INDEXED)))


class Screen:
    """Small VT text recorder for the cursor/erase sequences emitted by Crossterm."""
    def __init__(self, columns, rows, pixel_enabled=True):
        self.columns, self.rows = columns, rows
        self.cells = [[" "] * columns for _ in range(rows)]
        self.backgrounds = [[None] * columns for _ in range(rows)]
        self.color_cells = {}
        self.foreground = self.background = None
        self.pixel_enabled = pixel_enabled
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        value = self.pending + self.decoder.decode(data)
        self.pending = ""
        index = 0
        while index < len(value):
            char = value[index]
            if char == "\x1b":
                if index + 1 == len(value):
                    self.pending = value[index:]
                    break
                if value[index + 1] == "[":
                    match = CSI.match(value, index)
                    if not match:
                        self.pending = value[index:]
                        break
                    self.control(match.group(1), match.group(3))
                    index = match.end()
                    continue
                index += 2
                continue
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(self.y + 1, self.rows - 1)
            elif ord(char) >= 32:
                width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
                if self.x < self.columns:
                    if self.background is not None:
                        if not (self.pixel_enabled and self.columns >= HEADER_MIN_COLUMNS
                                and self.rows >= MASCOT_MIN_ROWS
                                and 2 <= self.x < 2 + MASCOT_WIDTH
                                and 1 <= self.y < 1 + MASCOT_HEIGHT
                                and char == "▀" and pixel_color(self.foreground)
                                and pixel_color(self.background)):
                            raise RuntimeError(f"background leaked outside opaque mascot cell: "
                                               f"{self.x},{self.y} {char!r} {self.background}")
                    self.cells[self.y][self.x] = char
                    self.backgrounds[self.y][self.x] = self.background
                    if self.foreground is not None or self.background is not None:
                        self.color_cells[self.x, self.y] = {
                            "x": self.x, "y": self.y,
                            "fg": self.foreground, "bg": self.background}
                    else:
                        self.color_cells.pop((self.x, self.y), None)
                    if width == 2 and self.x + 1 < self.columns:
                        self.cells[self.y][self.x + 1] = ""
                        self.backgrounds[self.y][self.x + 1] = self.background
                self.x = min(self.x + width, self.columns)
            index += 1

    def control(self, parameters, command):
        if parameters.startswith("?"):
            return
        values = [int(v) if v else 0 for v in parameters.split(";")]
        first = values[0] or 1
        if command == "m":
            index = 0
            while index < len(values):
                value = values[index]
                if value == 0:
                    self.foreground = self.background = None
                elif value == 39:
                    self.foreground = None
                elif value == 49:
                    self.background = None
                elif value in (38, 48):
                    if values[index + 1] == 2:
                        color = ("rgb", *values[index + 2:index + 5])
                        index += 4
                    elif values[index + 1] == 5:
                        color = ("indexed", values[index + 2])
                        index += 2
                    else:
                        raise RuntimeError("unsupported extended color sequence")
                    if value == 38:
                        self.foreground = color
                    else:
                        self.background = color
                index += 1
        elif command in ("H", "f"):
            self.y = min(first - 1, self.rows - 1)
            self.x = min((values[1] or 1) - 1 if len(values) > 1 else 0, self.columns - 1)
        elif command == "G":
            self.x = min(first - 1, self.columns - 1)
        elif command == "d":
            self.y = min(first - 1, self.rows - 1)
        elif command == "A":
            self.y = max(0, self.y - first)
        elif command == "B":
            self.y = min(self.rows - 1, self.y + first)
        elif command == "C":
            self.x = min(self.columns - 1, self.x + first)
        elif command == "D":
            self.x = max(0, self.x - first)
        elif command == "J":
            if values[0] == 2:
                self.cells = [[" "] * self.columns for _ in range(self.rows)]
                self.backgrounds = [[self.background] * self.columns for _ in range(self.rows)]
                self.color_cells.clear()
            elif values[0] == 0:
                self.cells[self.y][self.x:] = [" "] * (self.columns - self.x)
                self.backgrounds[self.y][self.x:] = [self.background] * (self.columns - self.x)
                self.color_cells = {(x, y): style for (x, y), style in self.color_cells.items()
                                    if y < self.y or (y == self.y and x < self.x)}
                for row in range(self.y + 1, self.rows):
                    self.cells[row] = [" "] * self.columns
                    self.backgrounds[row] = [self.background] * self.columns
        elif command == "K":
            if values[0] == 2:
                self.cells[self.y] = [" "] * self.columns
                self.backgrounds[self.y] = [self.background] * self.columns
                self.color_cells = {(x, y): style for (x, y), style in self.color_cells.items()
                                    if y != self.y}
            elif values[0] == 0:
                self.cells[self.y][self.x:] = [" "] * (self.columns - self.x)
                self.backgrounds[self.y][self.x:] = [self.background] * (self.columns - self.x)
                self.color_cells = {(x, y): style for (x, y), style in self.color_cells.items()
                                    if y != self.y or x < self.x}

    def resize(self, columns, rows):
        cells = [[" "] * columns for _ in range(rows)]
        backgrounds = [[None] * columns for _ in range(rows)]
        for y in range(min(self.rows, rows)):
            for x in range(min(self.columns, columns)):
                cells[y][x] = self.cells[y][x]
                backgrounds[y][x] = self.backgrounds[y][x]
        self.cells, self.backgrounds = cells, backgrounds
        self.columns, self.rows = columns, rows
        self.color_cells = {(x, y): style for (x, y), style in self.color_cells.items()
                            if x < columns and y < rows}
        self.x, self.y = min(self.x, columns - 1), min(self.y, rows - 1)

    def colored_background_count(self):
        return sum(color is not None for row in self.backgrounds for color in row)

    def lines(self):
        return ["".join(row).rstrip() for row in self.cells]

    def text(self):
        return "\n".join(self.lines())
