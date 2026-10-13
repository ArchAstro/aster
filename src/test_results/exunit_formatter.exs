defmodule AsterTestResults.ExUnitFormatter do
  @moduledoc false
  # Loaded by Aster with `elixir -r` for one `mix test` run when test results
  # are recorded. Each finished test becomes one JSON line appended to the
  # file named by ASTER_EXUNIT_REPORT. It never raises into the suite: a
  # test it cannot describe is left out, and Aster then marks the run's
  # results incomplete because its counts no longer match ExUnit's summary.

  use GenServer

  @impl true
  def init(_opts), do: {:ok, System.get_env("ASTER_EXUNIT_REPORT")}

  @impl true
  def handle_cast({:test_finished, %ExUnit.Test{} = test}, path) when is_binary(path) do
    try do
      case line(test) do
        nil -> :ok
        line -> File.write(path, [line, ?\n], [:append])
      end
    rescue
      _ -> :ok
    catch
      _, _ -> :ok
    end

    {:noreply, path}
  end

  def handle_cast(_event, state), do: {:noreply, state}

  defp line(test) do
    case outcome(test) do
      nil ->
        nil

      {outcome, message} ->
        encode([
          {"file", relative(test.tags[:file])},
          {"suite", inspect(test.module)},
          {"name", to_string(test.name)},
          {"outcome", outcome},
          {"duration_ms", div(test.time || 0, 1000)},
          {"message", message}
        ])
    end
  end

  defp outcome(%{state: nil}), do: {"pass", nil}
  defp outcome(%{state: {:skipped, _}}), do: {"skip", nil}
  defp outcome(%{state: {:skip, _}}), do: {"skip", nil}
  defp outcome(%{state: {:excluded, _}}), do: nil
  defp outcome(%{state: {:invalid, _}}), do: {"error", "setup_all failed for this module"}

  defp outcome(%{state: {:failed, failures}} = test) do
    kind = if Enum.all?(failures, &assertion?/1), do: "fail", else: "error"
    message = ExUnit.Formatter.format_test_failure(test, failures, 1, 100, fn _, msg -> msg end)
    {kind, message}
  end

  defp outcome(_test), do: nil

  defp assertion?({:error, %ExUnit.AssertionError{}, _stack}), do: true
  defp assertion?(_failure), do: false

  defp relative(nil), do: nil
  defp relative(file), do: Path.relative_to_cwd(file)

  defp encode(fields) do
    body =
      fields
      |> Enum.reject(fn {_key, value} -> is_nil(value) end)
      |> Enum.map(fn {key, value} -> [?", key, ?", ?:, value(value)] end)
      |> Enum.intersperse(?,)

    IO.iodata_to_binary([?{, body, ?}])
  end

  defp value(value) when is_integer(value), do: Integer.to_string(value)
  defp value(value) when is_binary(value), do: [?", escape(value), ?"]

  defp escape(text) do
    text = if String.valid?(text), do: text, else: inspect(text)

    for <<char::utf8 <- text>>, into: "" do
      case char do
        ?" -> "\\\""
        ?\\ -> "\\\\"
        ?\n -> "\\n"
        ?\r -> "\\r"
        ?\t -> "\\t"
        char when char < 0x20 -> "\\u" <> String.pad_leading(Integer.to_string(char, 16), 4, "0")
        char -> <<char::utf8>>
      end
    end
  end
end
