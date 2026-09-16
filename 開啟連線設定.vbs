Option Explicit
Dim shell, files, root, python, script
Set shell = CreateObject("WScript.Shell")
Set files = CreateObject("Scripting.FileSystemObject")
root = files.GetParentFolderName(WScript.ScriptFullName)
python = files.BuildPath(root, ".venv\Scripts\pythonw.exe")
script = files.BuildPath(root, "local_files_gui.py")
If Not files.FileExists(python) Then
    MsgBox "Python environment not found: " & python, 16, "MCP-Local"
    WScript.Quit 1
End If
shell.CurrentDirectory = root
shell.Run Chr(34) & python & Chr(34) & " -B " & Chr(34) & script & Chr(34), 0, False
